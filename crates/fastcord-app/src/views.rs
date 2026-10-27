//! What `App` draws (#169): the window, the settings screen and the screen
//! of the current phase. The menus and the account screen's parts are in
//! `views/`.

mod menus;

use crate::*;
use iced::widget::{column, row};

impl App {
    pub(crate) fn view(&self) -> Element<'_, Message> {
        let body = if self.audio_settings.open {
            self.settings_view()
        } else {
            self.view_body()
        };
        #[cfg(feature = "fake-discord")]
        if let Some(banner) = fake::banner() {
            return column![banner, body].into();
        }
        body
    }

    /// The settings screen with its current page.
    pub(crate) fn settings_view(&self) -> Element<'_, Message> {
        let account = match &self.phase {
            Phase::Account {
                session,
                persistence,
                gateway,
            } => Some((session, persistence, gateway)),
            _ => None,
        };
        let section = match self.settings_section {
            s if s.needs_account() && account.is_none() => settings::Section::Voice,
            s => s,
        };
        let controls = account.map(|(_, _, gateway)| gateway.controls.voice_controls());
        let missing = || -> Element<'_, Message> { ui::note(i18n::tr("Not available yet.")) };
        let content: Element<'_, Message> = match (section, account) {
            (settings::Section::Profile, Some((session, _, gateway))) => profile_edit::view(
                &self.ui.identity,
                &gateway.navigation,
                own_user(gateway, session),
            )
            .map_or_else(missing, |panel| panel.map(Message::Identity)),
            (settings::Section::Accounts, Some((session, persistence, _))) => account_panel::view(
                &self.ui.account_panel,
                &self.account_index,
                session.user.id,
                *persistence == Persistence::Saved,
            )
            .map_or_else(missing, |panel| panel.map(Message::Account)),
            (settings::Section::Privacy, Some((_, _, gateway))) => {
                safety::view(&self.ui.safety, &gateway.navigation)
                    .map_or_else(missing, |panel| panel.map(Message::Safety))
            }
            (settings::Section::Chat, _) => chat_prefs::view(
                self.audio_preferences.chat,
                &chat_prefs::SpeechView {
                    prefs: &self.audio_preferences.speech,
                    voices: &self.speech_voices,
                    error: speech::error(),
                    speaking: speech::speaking(),
                },
            ),
            (settings::Section::Notifications, _) => {
                audio_devices::notifications(&self.audio_preferences)
            }
            (settings::Section::Network, _) => network_ui::view(
                &self.network_settings,
                &self.network,
                matches!(self.phase, Phase::Login),
            ),
            (settings::Section::App, _) => column![
                desktop::view(
                    self.audio_preferences.close_to_tray,
                    &self.tray,
                    self.autostart,
                ),
                audio_devices::experimental(&self.audio_preferences),
                unverified::view(
                    account.map(|(session, _, _)| &session.client),
                    self.ui.unverified_open,
                ),
                audio_devices::diagnostics(
                    controls.as_ref().and_then(|c| c.audio_stats),
                    controls.as_ref().map_or_else(String::new, |c| {
                        audio_devices::transmit_summary(
                            c.audio_stats,
                            c.transport_stats,
                            c.capture_enabled(),
                        )
                    }),
                ),
            ]
            .spacing(28)
            .into(),
            _ => column![
                audio_devices::devices(
                    &self.audio_settings,
                    &self.audio_preferences,
                    controls.as_ref().and_then(|c| c.input_error),
                    controls.as_ref().and_then(|c| c.output_error),
                ),
                mic_panel::view(
                    &self.mic_panel,
                    self.audio_preferences.input_gain,
                    self.call_active(),
                    (self.audio_preferences.voice_input.mode
                        == shortcuts::InputMode::VoiceActivity)
                        .then_some(self.audio_preferences.voice_input.sensitivity)
                        .flatten(),
                ),
                shortcuts::view(
                    &self.audio_preferences.voice_input,
                    &self.shortcuts,
                    controls.as_ref().is_some_and(|c| c.vad_forbidden),
                    self.call_active(),
                ),
                audio_devices::processing(
                    &self.audio_preferences,
                    controls.as_ref().and_then(|c| c.noise_status),
                ),
            ]
            .spacing(28)
            .into(),
        };
        let content = match self.notice.as_deref() {
            Some(notice) if account.is_none() => column![ui::error(i18n::tr(notice)), content]
                .spacing(16)
                .into(),
            _ => content,
        };
        settings::view(section, account.is_some(), content)
    }

    pub(crate) fn view_body(&self) -> Element<'_, Message> {
        if self.compact_call
            && let Some(gateway) = self.phase.gateway()
        {
            return desktop::compact_view(&gateway.controls.voice_controls());
        }
        if let Some(gateway) = self.phase.gateway()
            && let Some((guild, section)) = self.server_page()
        {
            return self.server_screen(gateway, guild, section);
        }
        if let Phase::Account {
            session,
            persistence,
            gateway,
        } = &self.phase
        {
            let voice_audio = gateway.controls.voice_audio_state();
            let voice_controls = gateway.controls.voice_controls();
            let search_bar: Element<'_, Message> = match Self::search_target(gateway) {
                Some((guild, _)) => {
                    search::view(&self.ui.search, guild.is_some()).map(Message::Search)
                }
                None => text("").size(1).into(),
            };
            let watching = gateway.controls.watched();
            let video_tiles = call_layout::tiles(
                &watching,
                &gateway.controls.camera_tiles().snapshot(),
                voice_controls.camera.wanted,
            );
            let video_plan = gateway.call_view.plan(
                &call_layout::ids(&video_tiles),
                &call_ui::speaking(&voice_controls),
            );
            let in_voice = voice_audio != gateway::VoiceAudioState::Idle;
            let running = voice_audio == gateway::VoiceAudioState::Running;
            let bold = iced::Font {
                weight: iced::font::Weight::Bold,
                ..iced::Font::DEFAULT
            };

            // ---- the voice panel and its settings, above the account footer ----
            let share = gateway.controls.share_view();
            let voice_panel: Option<Element<'_, Message>> = in_voice.then(|| {
                let place = match (gateway.voice_guild, gateway.voice_call) {
                    (Some(_), _) => {
                        let channel = gateway.controls.voice_target().map(|(_, channel)| channel);
                        let name = gateway
                            .navigation
                            .channels
                            .rows
                            .iter()
                            .find(|row| Some(row.id) == channel && row.occupant.is_none())
                            .map(|row| row.name.as_str());
                        match (name, gateway.navigation.selected_guild_name.as_str()) {
                            (Some(name), "") => name.to_owned(),
                            (Some(name), guild) => format!("{name} / {guild}"),
                            (None, _) => "Voice channel".to_owned(),
                        }
                    }
                    (None, Some(call)) => gateway
                        .navigation
                        .private_channels
                        .rows
                        .iter()
                        .find(|row| row.id == call)
                        .map_or_else(|| "Private call".to_owned(), |row| row.label.clone()),
                    (None, None) => String::new(),
                };
                call_ui::bar(call_ui::Bar {
                    state: voice_audio,
                    place,
                    camera: gateway::can_send_camera().then_some((
                        voice_controls.camera.wanted,
                        running || voice_controls.camera.wanted,
                    )),
                    share: cfg!(windows).then(|| {
                        if share.active {
                            (true, true)
                        } else {
                            (
                                false,
                                running
                                    && (gateway.voice_guild.is_some()
                                        || gateway.voice_call.is_some())
                                    && !gateway.share_picker.is_open(),
                            )
                        }
                    }),
                    soundboard: self.soundboard_toggle(gateway),
                    settings_open: self.ui.voice_details,
                    can_leave: gateway.voice_guild.or(gateway.voice_call).is_some(),
                    share_text: share.text.clone(),
                    ping_ms: voice_controls.voice_ping_ms,
                })
            });
            let voice_settings: Option<Element<'_, Message>> = (in_voice && self.ui.voice_details)
                .then(|| {
                    let saved = &self.audio_preferences.camera;
                    let camera = &voice_controls.camera;
                    call_ui::settings(call_ui::Settings {
                        camera: gateway::can_send_camera().then(|| call_ui::CameraSettings {
                            choices: self.camera_list.choices(saved),
                            selected: self.camera_list.selected(saved),
                            effect: saved.effect,
                            mirror: saved.mirror,
                            probing: self.camera_list.probing,
                            status: match &self.camera_list.error {
                                Some(error) if camera.status == gateway::CameraStatus::Off => {
                                    error.clone()
                                }
                                _ => gateway::describe_camera(camera),
                            },
                        }),
                        voice: &voice_controls,
                        volume_notice: self.volume_notice.as_deref(),
                        watching: watching
                            .iter()
                            .map(|session| {
                                let owner = call_layout::stream_owner(&session.key).map_or_else(
                                    || "stream".to_owned(),
                                    |owner| voice_controls.display_name(owner),
                                );
                                (
                                    session.clone(),
                                    format!("{owner}: {}", session.description()),
                                )
                            })
                            .collect(),
                        offers: voice_controls
                            .streams
                            .iter()
                            .filter(|offer| {
                                !watching.iter().any(|session| session.key == offer.key)
                            })
                            .map(|offer| {
                                (offer.key.clone(), voice_controls.display_name(offer.owner))
                            })
                            .collect(),
                        running,
                    })
                });
            let mut talking = call_ui::speaking(&voice_controls);
            if voice_controls.self_speaking {
                talking.push(session.user.id);
            }

            // ---- connection and notices above the conversation ----
            let mut notes: Vec<(String, bool)> = Vec::new();
            let mut say = |line: String, danger: bool| {
                if !line.is_empty() {
                    notes.push((line, danger));
                }
            };
            if !matches!(gateway.status, gateway::GatewayStatus::Ready(_)) {
                say(
                    gateway.status.describe(),
                    matches!(gateway.status, gateway::GatewayStatus::Stopped(_)),
                );
            }
            if let Some(error) = voice_controls.input_error {
                say(
                    i18n::trf(
                        "Microphone stopped: {}. Open the voice settings to try again.",
                        &[&i18n::tr(&error.to_string())],
                    ),
                    true,
                );
            }
            if let Some(error) = voice_controls.output_error {
                say(
                    i18n::trf(
                        "Playback stopped: {}. Open the voice settings to try again.",
                        &[&i18n::tr(&error.to_string())],
                    ),
                    true,
                );
            }
            say(self.notice.clone().unwrap_or_default(), false);
            say(gateway.attachment_notice.unwrap_or("").to_owned(), false);
            // Closed, search is a tool in the conversation header.
            let has_search = self.ui.search.open && !search_bar_is_empty(gateway);
            let mut banner_column = column![].spacing(2);
            for (label, role) in [
                ("Voice calls", fastcord_net::Role::Voice),
                ("Stream", fastcord_net::Role::Stream),
            ] {
                let status = self.network.controller().service_status(role);
                if matches!(
                    status,
                    fastcord_net::selector::ServiceStatus::UdpDiscovering { .. }
                        | fastcord_net::selector::ServiceStatus::UdpUnreachable
                        | fastcord_net::selector::ServiceStatus::Failed(
                            fastcord_net::NetworkError::ProxyUdpClosed
                                | fastcord_net::NetworkError::ProxyUdpUnavailable
                        )
                ) {
                    banner_column = banner_column.push(
                        row![
                            text(format!(
                                "{}: {}",
                                i18n::tr(label),
                                network_ui::status_text(status)
                            ))
                            .size(13),
                            button(text(i18n::tr("Network settings")).size(12))
                                .on_press(Message::Network(network_ui::Action::Open))
                                .style(style::secondary_button(false)),
                        ]
                        .spacing(8),
                    );
                }
            }
            if has_search {
                banner_column = banner_column.push(search_bar);
            }
            if let Some(channel) = gateway.timeline.channel_id
                && self.ui.pins.shows(channel)
            {
                banner_column = banner_column.push(pins::view(&self.ui.pins).map(Message::Pins));
            }
            if let Some(panel) = soundboard::view(&self.ui.soundboard) {
                banner_column = banner_column.push(panel.map(Message::Soundboard));
            }
            if let Some(panel) = stage::banner(
                &self.ui.stage,
                &voice_controls,
                session.client.writes_restricted(),
            ) {
                banner_column = banner_column.push(panel);
            }
            if let Some(channel) = gateway.timeline.channel_id
                && let Some(panel) = slash::view(
                    &self.ui.slash,
                    gateway
                        .composer
                        .slash_query()
                        .filter(|_| gateway.timeline.can_send)
                        .as_deref(),
                    Self::slash_scope(gateway, channel),
                    &gateway.timeline.interaction,
                    std::time::Instant::now(),
                )
            {
                banner_column = banner_column.push(panel.map(Message::Slash));
            }
            if let Some(channel) = gateway.timeline.channel_id
                && self.ui.poll.is_open_for(channel)
                && let Some(panel) = poll_create::view(&self.ui.poll)
            {
                banner_column = banner_column.push(panel.map(Message::Poll));
            }
            if let Some(channel) = gateway.timeline.channel_id
                && let Some(panel) =
                    voice_record::view(&self.ui.voice, &self.voice_context(gateway, channel))
            {
                banner_column = banner_column.push(panel.map(Message::Voice));
            }
            // Forms open as dialogs over the window; only their reminder
            // lines and notices stay above the conversation.
            let mut dialogs: Vec<(Element<'_, Message>, Option<Message>)> = Vec::new();
            if let Some(panel) =
                onboarding::view(&self.ui.onboarding, &gateway.navigation, &gateway.revealed)
            {
                let panel = panel.map(Message::Onboarding);
                if self.ui.onboarding.is_open() {
                    dialogs.push((panel, Some(Message::Onboarding(onboarding::Event::Close))));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = scheduled_events::view(&self.ui.events, &gateway.navigation) {
                let panel = panel.map(Message::Events);
                if self
                    .ui
                    .events
                    .is_open_for(gateway.navigation.selected_guild)
                {
                    dialogs.push((panel, Some(Message::Events(scheduled_events::Event::Close))));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = guild_admin::view(&self.ui.guild_admin, &gateway.navigation) {
                let panel = panel.map(Message::GuildAdmin);
                if self.ui.guild_admin.is_dialog() {
                    dialogs.push((panel, Some(Message::GuildAdmin(guild_admin::Event::Close))));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = announce::view(&self.ui.announce, &gateway.navigation) {
                dialogs.push((
                    panel.map(Message::Announce),
                    Some(Message::Announce(announce::Event::Close)),
                ));
            }
            if let Some(panel) = apps::view(&self.ui.apps, &gateway.navigation) {
                // The account's authorized apps; server pages are a screen.
                dialogs.push((
                    panel.map(Message::Apps),
                    Some(Message::Apps(apps::Event::Close)),
                ));
            }
            if let Some(panel) = invite::view(&self.ui.invite, localtime::now()) {
                let panel = panel.map(Message::Invite);
                if self.ui.invite.is_dialog() {
                    dialogs.push((panel, self.ui.invite.dismiss().map(Message::Invite)));
                } else {
                    banner_column = banner_column.push(panel);
                }
            }
            if let Some(panel) = discover::view(
                &self.ui.discover,
                &gateway.navigation,
                &gateway.avatars.views,
            ) {
                dialogs.push((
                    panel.map(Message::Discover),
                    Some(Message::Discover(discover::Event::Toggle)),
                ));
            }
            if let Some(panel) =
                group_dm::view(&self.ui.group, &gateway.navigation, session.user.id)
            {
                dialogs.push((
                    panel.map(Message::Group),
                    Some(Message::Group(group_dm::Event::Close)),
                ));
            }
            if let Some(panel) = report::view(&self.ui.report, &gateway.navigation) {
                dialogs.push((
                    panel.map(Message::Report),
                    Some(Message::Report(report::Event::Close)),
                ));
            }
            if self.ui.forward.is_open()
                && let Some(panel) = forward::view(&self.ui.forward, &forward_candidates(gateway))
            {
                dialogs.push((
                    panel.map(Message::Forward),
                    Some(Message::Forward(forward::Event::Close)),
                ));
            }
            // One at a time: the first open one.
            let dialog = dialogs
                .into_iter()
                .next()
                .map(|(panel, close)| ui::modal_maybe(ui::sheet(panel, 640.0), close));
            if let Some(panel) = threads::view(&self.ui.threads) {
                banner_column = banner_column.push(panel.map(Message::Threads));
            }
            for (line, danger) in notes {
                let line = text(i18n::tr(&line).to_owned()).size(12);
                banner_column = banner_column.push(if danger {
                    line.style(text::danger)
                } else {
                    line.color(crate::style::muted())
                });
            }
            if matches!(
                &gateway.status,
                gateway::GatewayStatus::Stopped(reason) if reason.allows_manual_reconnect()
            ) {
                banner_column = banner_column.push(
                    button(text(i18n::tr("Reconnect")).size(12))
                        .on_press(Message::ReconnectGateway)
                        .style(style::action_button(false)),
                );
            }
            for call in &gateway.navigation.incoming_calls {
                // Answering joins voice; a call already joined has no prompt.
                if gateway.voice_call == Some(call.channel_id) && in_voice {
                    continue;
                }
                banner_column = banner_column.push(
                    container(
                        row![
                            text(i18n::trf("Incoming call: {}", &[&call.label]))
                                .size(13)
                                .font(bold)
                                .width(Length::Fill),
                            primary_button(text(i18n::tr("Accept")).size(12))
                                .on_press(Message::AcceptCall(call.channel_id))
                                .style(style::action_button(false)),
                            button(text(i18n::tr("Decline")).size(12))
                                .on_press(Message::DeclineCall(call.channel_id))
                                .style(style::action_button(true)),
                        ]
                        .spacing(8)
                        .align_y(iced::alignment::Vertical::Center),
                    )
                    .padding([6, 10])
                    .style(style::card),
                );
            }
            if session.client.writes_restricted() {
                banner_column = banner_column.push(text(i18n::tr("Discord requires account verification. Sending is paused; open the official Discord client, complete its check, then reconnect fastcord.")).size(13).style(text::danger));
            }
            let banner: Option<Element<'_, Message>> = Some(banner_column.into());

            // ---- the user's panel at the foot of the sidebar ----
            // Avatar and name (status, profile, accounts behind a click),
            // microphone with its device menu, headphones, and the settings.
            let me = own_user(gateway, session);
            let display = me.display_name().to_owned();
            let own = gateway.navigation.presences.get(&session.user.id);
            let own_status = self.ui.status.status(own);
            let mut foot = column![];
            if let Some(settings) = voice_settings {
                foot = foot.push(settings);
            }
            if let Some(panel) = stage::own(
                &self.ui.stage,
                &voice_controls,
                session.client.writes_restricted(),
            ) {
                foot = foot.push(panel);
            }
            if let Some(panel) = voice_panel {
                foot = foot.push(panel);
            }
            let identity = button(
                row![
                    avatars::view(
                        &gateway.avatars.views,
                        &avatars::AvatarKey::user(me.id, me.avatar.as_deref()),
                        32.0,
                        || ui::initials(&display, session.user.id.0, 32.0),
                    ),
                    column![
                        text(display.clone())
                            .size(13)
                            .font(bold)
                            .wrapping(text::Wrapping::None),
                        row![
                            status::dot(own_status),
                            text(
                                self.ui
                                    .status
                                    .custom(own)
                                    .or(own_status.map(status::label))
                                    .unwrap_or(i18n::tr("Set status"))
                                    .to_owned()
                            )
                            .size(11)
                            .color(style::MUTED)
                            .wrapping(text::Wrapping::None),
                        ]
                        .spacing(5)
                        .align_y(iced::alignment::Vertical::Center),
                    ]
                    .width(Length::Fill),
                ]
                .spacing(8)
                .align_y(iced::alignment::Vertical::Center),
            )
            .on_press(Message::Status(status::Event::ToggleMenu))
            .width(Length::Fill)
            .padding([2, 4])
            .style(style::row_button(self.ui.status.open, false));
            let action = |icon, on_press, active, danger, tip: &str| icons::Action {
                icon,
                on_press,
                active,
                danger,
                tip: tip.to_owned(),
                badge: 0,
            };
            let mic = icons::action(action(
                icons::Icon::Mic {
                    muted: voice_controls.self_muted,
                },
                running.then_some(Message::SetSelfMute(!voice_controls.self_muted)),
                !voice_controls.self_muted,
                voice_controls.self_muted,
                if !running {
                    i18n::tr("Microphone (available in a call)")
                } else if voice_controls.self_muted {
                    i18n::tr("Turn microphone on")
                } else {
                    i18n::tr("Turn microphone off")
                },
            ));
            let devices = iced::widget::tooltip(
                button(icons::sized(icons::Icon::ChevronUp, style::MUTED, 12.0))
                    .on_press(Message::MicMenu(!self.mic_menu))
                    .padding([8, 1])
                    .style(style::tool_button(self.mic_menu, false)),
                container(text(i18n::tr("Input and output devices")).size(11))
                    .padding([3, 6])
                    .style(container::rounded_box),
                iced::widget::tooltip::Position::Top,
            );
            let headphones = icons::action(action(
                icons::Icon::Headphones {
                    deafened: voice_controls.deafened,
                },
                running.then_some(Message::SetDeafen(!voice_controls.deafened)),
                !voice_controls.deafened,
                voice_controls.deafened,
                if !running {
                    i18n::tr("Sound (available in a call)")
                } else if voice_controls.deafened {
                    i18n::tr("Turn sound on")
                } else {
                    i18n::tr("Turn sound off")
                },
            ));
            let gear = icons::action(action(
                icons::Icon::Settings,
                Some(Message::Settings(settings::Event::Open(
                    self.settings_section,
                ))),
                true,
                false,
                i18n::tr("Settings"),
            ));
            foot = foot.push(
                container(
                    row![
                        container(identity).width(Length::Fill).clip(true),
                        row![mic, devices, headphones, gear]
                            .spacing(0)
                            .align_y(iced::alignment::Vertical::Center),
                    ]
                    .spacing(2)
                    .align_y(iced::alignment::Vertical::Center),
                )
                .padding([6, 6])
                .style(style::footer),
            );
            // The menu behind the name: status, profile, accounts, log out.
            let account_menu: Option<Element<'_, Message>> = self.ui.status.open.then(|| {
                let pick = |message| Message::MenuPick(Box::new(message));
                ui::popover(
                    ui::menu_card(
                        column![
                            row![
                                ui::initials(&display, session.user.id.0, 40.0),
                                column![
                                    text(display.clone()).size(15).font(bold),
                                    text(format!("@{}", me.username))
                                        .size(12)
                                        .color(style::MUTED),
                                ],
                            ]
                            .spacing(10)
                            .align_y(iced::alignment::Vertical::Center)
                            .padding(6),
                            self.ui
                                .status
                                .menu(
                                    own,
                                    if *persistence == Persistence::Saved {
                                        i18n::tr("Saved login")
                                    } else {
                                        i18n::tr("Not saved on this device")
                                    },
                                )
                                .map(Message::Status),
                            ui::separator(),
                            ui::item(
                                Some(icons::Icon::Edit),
                                i18n::tr("Edit profile"),
                                Some(pick(Message::Settings(settings::Event::Open(
                                    settings::Section::Profile
                                )))),
                            ),
                            ui::item(
                                Some(icons::Icon::Group),
                                i18n::tr("Switch account"),
                                Some(pick(Message::Settings(settings::Event::Open(
                                    settings::Section::Accounts
                                )))),
                            ),
                            ui::separator(),
                            ui::danger_item(
                                Some(icons::Icon::Exit),
                                i18n::tr("Log out"),
                                Some(pick(Message::Account(account_panel::Event::Logout))),
                            ),
                        ]
                        .spacing(4),
                        300.0,
                    ),
                    ui::Anchor::BottomLeft(navigation::RAIL_WIDTH + 8.0, 60.0),
                    Message::Status(status::Event::ToggleMenu),
                )
            });
            // Quick device choice above the microphone.
            let mic_menu: Option<Element<'_, Message>> = self.mic_menu.then(|| {
                ui::popover(
                    ui::menu_card(
                        column![
                            audio_devices::devices(
                                &self.audio_settings,
                                &self.audio_preferences,
                                voice_controls.input_error,
                                voice_controls.output_error,
                            ),
                            ui::separator(),
                            ui::item(
                                Some(icons::Icon::Settings),
                                i18n::tr("Voice settings"),
                                Some(Message::MenuPick(Box::new(Message::Settings(
                                    settings::Event::Open(settings::Section::Voice)
                                )))),
                            ),
                        ]
                        .spacing(8)
                        .padding(8),
                        320.0,
                    ),
                    ui::Anchor::BottomLeft(navigation::RAIL_WIDTH + 8.0, 60.0),
                    Message::MicMenu(false),
                )
            });

            let account = container(navigation::view(
                &gateway.navigation,
                &gateway.timeline,
                &gateway.composer,
                {
                    let mut interaction = gateway.interaction();
                    interaction.publishing = self.ui.announce.publishing();
                    interaction
                },
                timeline::Attachments {
                    account: session.user.id,
                    views: &gateway.attachment_views,
                    media: gateway.media.as_ref(),
                    emojis: &gateway.emoji.views,
                    avatars: &gateway.avatars.views,
                    revealed: &gateway.revealed,
                    invites: &self.ui.invite.cache,
                    members: if gateway.directs {
                        &NO_MEMBERS
                    } else {
                        &gateway.navigation.members
                    },
                },
                navigation::Chrome {
                    directs: gateway.directs,
                    footer: foot.into(),
                    banner,
                    speaking: talking,
                    voice_call: gateway.voice_call.filter(|_| in_voice),
                    wide: self.wide(),
                    forum: gateway.navigation.forum.as_ref().map(|forum| {
                        forum::view(
                            &self.ui.forum,
                            forum,
                            self.ui.threads.forum_archive(forum.channel),
                            session.user.id,
                            localtime::now(),
                        )
                    }),
                    inbox: self.ui.notify_settings.is_open().then(|| {
                        notify_settings::view(
                            &self.ui.notify_settings,
                            &gateway.navigation.notifications,
                            gateway.directs,
                        )
                        .map(Message::NotifySettings)
                    }),
                    friends: if self.ui.requests_open && self.ui.safety.is_open() {
                        safety::view(&self.ui.safety, &gateway.navigation)
                            .map(|panel| container(panel.map(Message::Safety)).padding(16).into())
                    } else {
                        None
                    }
                    .or_else(|| {
                        self.ui.friends.open.then(|| {
                            friends::view(
                                &self.ui.friends,
                                &gateway.navigation.relationships,
                                &gateway.navigation.presences,
                            )
                            .map(Message::Friends)
                        })
                    }),
                },
            ))
            .height(Length::Fill);
            // A profile popup covers whatever is under it.
            let account: Element<'_, Message> = match friends::profile_view(
                &self.ui.friends,
                &gateway.navigation.relationships,
                session.user.id,
                friends::MemberContext::of(
                    self.ui.friends.profile_user(),
                    &gateway.navigation,
                    gateway.directs,
                ),
            ) {
                Some(popup) => iced::widget::stack![account, popup.map(Message::Friends)].into(),
                None => account.into(),
            };
            // The larger image view covers the account screen, which stays in
            // the widget tree underneath so the timeline keeps its scroll state.
            let screen: Element<'_, Message> = match &gateway.attachment_viewer {
                None if gateway.call_view.open && !gateway.emoji_picker.request.open => {
                    let pane = call_ui::panel(
                        &gateway.call_view,
                        &video_tiles,
                        &video_plan,
                        &voice_controls,
                        self.window_fullscreen,
                        self.notice.as_deref(),
                    );
                    iced::widget::stack![account, pane].into()
                }
                None if gateway.share_picker.is_open() && !gateway.emoji_picker.request.open => {
                    iced::widget::stack![
                        account,
                        Self::share_picker_view(
                            &gateway.share_picker,
                            gateway.share_audio,
                            &gateway.share_audio_sources,
                            gateway.share_audio_target,
                        )
                    ]
                    .into()
                }
                Some(viewer) => {
                    iced::widget::stack![account, attachment_ui::viewer_view(viewer)].into()
                }
                None if self.expressions_tab(gateway).is_some() => iced::widget::stack![
                    account,
                    container(self.expressions_view(gateway))
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .align_x(iced::alignment::Horizontal::Right)
                        .align_y(iced::alignment::Vertical::Bottom)
                        .padding(20),
                ]
                .into(),
                None => account,
            };
            let screen = match member_menu::view(
                &self.ui.member_menu,
                &gateway.navigation,
                voice_controls.clone(),
                session.client.writes_restricted(),
                self.volume_notice.as_deref(),
            ) {
                Some(menu) => iced::widget::stack![screen, menu].into(),
                None => screen,
            };
            let menu = match self.ui.menu {
                Some(Menu::Server(guild)) => Some(self.server_menu(gateway, guild)),
                Some(Menu::Conversation) => Some(self.conversation_menu(gateway)),
                Some(Menu::Composer) => Some(self.composer_menu(gateway)),
                None => None,
            };
            // The inbox floats beside the rail; the conversation stays put.
            let inbox_panel: Option<Element<'_, Message>> = self.ui.inbox.is_open().then(|| {
                ui::popover(
                    container(
                        inbox::view(&self.ui.inbox, &gateway.navigation.inbox).map(Message::Inbox),
                    )
                    .max_height(640)
                    .into(),
                    ui::Anchor::TopLeft(navigation::RAIL_WIDTH + 4.0, 8.0),
                    Message::Inbox(inbox::Event::Toggle),
                )
            });
            let screen = match account_menu
                .or(mic_menu)
                .or(menu)
                .or(inbox_panel)
                .or_else(|| {
                    new_dm::view(
                        &self.ui.new_dm,
                        &gateway.navigation.relationships,
                        gateway.private.notice.as_deref(),
                    )
                }) {
                Some(menu) => iced::widget::stack![screen, menu].into(),
                None => screen,
            };
            let screen = match dialog {
                Some(dialog) => iced::widget::stack![screen, dialog].into(),
                None => screen,
            };
            let overlay: Option<Element<'_, Message>> = if self.ui.switcher.is_open() {
                Some(
                    switcher::view(&self.ui.switcher, &gateway.navigation.quick, |channel| {
                        gateway.composer.has_draft(channel)
                    })
                    .map(Message::Switcher),
                )
            } else if self.ui.switcher.help_open() {
                Some(switcher::help_view().map(Message::Switcher))
            } else {
                None
            };
            return match overlay {
                Some(overlay) => iced::widget::stack![screen, overlay].into(),
                None => screen,
            };
        }
        let stage = match &self.phase {
            Phase::Starting => login_screen::Stage::Starting,
            Phase::Login => login_screen::Stage::Form,
            Phase::Qr(attempt) => login_screen::Stage::Qr(&attempt.stage),
            Phase::Mfa(_) => login_screen::Stage::Mfa,
            Phase::Authenticating { saved_account } => login_screen::Stage::Checking {
                restoring: saved_account.is_some(),
            },
            Phase::MemoryConsent { session, error } => login_screen::Stage::MemoryConsent {
                name: format!(
                    "{} (@{})",
                    session.user.display_name(),
                    session.user.username
                ),
                error: i18n::tr(&error.to_string()).to_owned(),
            },
            Phase::Account { .. } => unreachable!("account view returned above"),
            Phase::Deleting { .. } => login_screen::Stage::Deleting,
            Phase::DeleteFailed { error, .. } => login_screen::Stage::DeleteFailed {
                error: i18n::tr(&error.to_string()).to_owned(),
            },
        };
        login_screen::view(
            &self.login_ui,
            login_screen::Inputs {
                stage,
                saved: self
                    .saved_accounts
                    .iter()
                    .map(|id| {
                        (
                            *id,
                            self.account_index.get(*id).map_or_else(
                                || i18n::trf("Account {}", &[id]),
                                accounts::Entry::label,
                            ),
                        )
                    })
                    .collect(),
                acknowledged: self.acknowledged,
                memory_only: self.memory_only,
                token: self.token.expose_secret(),
                login: &self.password_login,
                password: self.password.expose_secret(),
                notice: self.notice.as_deref().map(i18n::tr),
                can_login: self.can_login(),
                can_password: self.can_password_login(),
                store_failed: self.store.is_none(),
            },
        )
    }
}
