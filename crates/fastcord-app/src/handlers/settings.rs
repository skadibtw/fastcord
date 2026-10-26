//! The settings screens: audio devices, speech, chat and notification preferences.

use crate::*;

impl App {
    pub(crate) fn settings_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Settings(settings::Event::Open(section)) => {
                let opening = !self.audio_settings.open;
                let moved = std::mem::replace(&mut self.settings_section, section) != section;
                self.network_settings.open = section == settings::Section::Network;
                // A new page starts at its title.
                let mut tasks = vec![if moved || opening {
                    iced::widget::operation::scroll_to(
                        settings::PAGE,
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    )
                } else {
                    Task::none()
                }];
                if opening {
                    tasks.push(self.handle(Message::OpenAudioSettings));
                }
                if let Phase::Account { .. } = &self.phase {
                    // The account pages are the existing panels, kept open
                    // while their page is shown.
                    if section == settings::Section::Profile && !self.ui.identity.is_open() {
                        tasks.push(self.handle(Message::Identity(profile_edit::Event::Toggle)));
                    }
                    if section == settings::Section::Accounts && !self.ui.account_panel.is_open() {
                        tasks.push(self.handle(Message::Account(account_panel::Event::Toggle)));
                    }
                    if section == settings::Section::Privacy {
                        if !self.ui.safety.is_open() {
                            tasks.push(self.handle(Message::Safety(safety::Event::Toggle)));
                        }
                        tasks.push(
                            self.handle(Message::Safety(safety::Event::Tab(safety::Tab::Privacy))),
                        );
                    }
                }
                return Task::batch(tasks);
            }
            Message::ServerSettings(guild, section) => {
                let Some(gateway) = self.phase.gateway() else {
                    return Task::none();
                };
                let voice = section.channel().is_some_and(|channel| {
                    gateway.navigation.admin.as_ref().is_some_and(|admin| {
                        admin.channels.iter().any(|c| {
                            c.id == channel && matches!(c.kind, AdminKind::Voice | AdminKind::Stage)
                        })
                    })
                });
                self.close_menus();
                let close = self.close_server_pages();
                use server_settings::Section;
                let open = self.handle(Message::GuildAdmin(match section {
                    Section::Overview => guild_admin::Event::Settings(guild),
                    Section::Roles => guild_admin::Event::Roles(guild),
                    Section::Expressions => guild_admin::Event::Assets(guild),
                    Section::Community => guild_admin::Event::Community(guild),
                    Section::AutoMod => guild_admin::Event::AutoMod(guild),
                    Section::Integrations => guild_admin::Event::Apps(guild),
                    Section::Bans => guild_admin::Event::Bans(guild),
                    Section::Audit => guild_admin::Event::Audit(guild),
                    Section::Channel(channel) => {
                        guild_admin::Event::EditChannel(guild, channel, voice)
                    }
                    Section::Webhooks(channel) => guild_admin::Event::Webhooks(guild, channel),
                }));
                return Task::batch([
                    close,
                    open,
                    iced::widget::operation::scroll_to(
                        settings::PAGE,
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    ),
                ]);
            }
            Message::CloseServerSettings => return self.close_server_pages(),
            Message::MicMenu(open) => {
                self.mic_menu = open;
                if open && self.audio_settings.inputs.is_empty() && !self.audio_settings.probing {
                    self.audio_settings.probing = true;
                    return Task::perform(audio_devices::enumerate(), Message::AudioDevicesListed);
                }
            }
            Message::Settings(settings::Event::Close) => {
                return self.handle(Message::CloseAudioSettings);
            }
            Message::OpenAudioSettings if !self.audio_settings.open => {
                self.close_menus();
                self.audio_settings.open = true;
                let voices = Task::perform(speech::load_voices(), |voices| {
                    Message::SpeechVoices(voices.unwrap_or_default())
                });
                return Task::batch([voices, self.handle(Message::RefreshAudioDevices)]);
            }
            Message::OpenAudioSettings | Message::RefreshAudioDevices => {
                if let Some(gateway) = self.phase.gateway()
                    && self.audio_settings.open
                {
                    gateway.controls.set_diagnostics_open(true);
                }
                if !self.audio_settings.probing {
                    self.audio_settings.probing = true;
                    return Task::perform(audio_devices::enumerate(), Message::AudioDevicesListed);
                }
            }
            Message::CloseAudioSettings => {
                self.audio_settings.open = false;
                self.network_settings.open = false;
                self.mic_panel.close();
                let mut closing = Vec::new();
                if self.ui.identity.is_open() {
                    closing.push(Message::Identity(profile_edit::Event::Toggle));
                }
                if self.ui.account_panel.is_open() {
                    closing.push(Message::Account(account_panel::Event::Toggle));
                }
                if self.ui.safety.is_open() {
                    closing.push(Message::Safety(safety::Event::Close));
                }
                if !closing.is_empty() {
                    let tasks: Vec<_> = closing.into_iter().map(|m| self.handle(m)).collect();
                    if let Some(gateway) = self.phase.gateway() {
                        gateway.controls.set_diagnostics_open(false);
                    }
                    return Task::batch(tasks);
                }
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.set_diagnostics_open(false);
                }
            }
            Message::MicPanel(event) => return self.mic_event(event),
            Message::AudioDevicesListed(result) => {
                self.audio_settings.probing = false;
                match result.input {
                    Ok(inputs) => {
                        self.audio_settings.inputs = inputs;
                        self.audio_settings.input_error = None;
                    }
                    Err(error) => self.audio_settings.input_error = Some(error),
                }
                match result.output {
                    Ok(outputs) => {
                        self.audio_settings.outputs = outputs;
                        self.audio_settings.output_error = None;
                    }
                    Err(error) => self.audio_settings.output_error = Some(error),
                }
            }
            Message::SelectAudioDevice(direction, option) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway
                        .controls
                        .select_audio_device(direction, option.choice.clone());
                }
                self.audio_preferences.select(direction, option);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::NoiseSuppressionChanged(mode) => {
                self.audio_preferences.noise_suppression = mode;
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.select_noise_suppression(mode);
                }
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::EchoCancellationChanged(enabled) => {
                self.audio_preferences.echo_cancellation = enabled;
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.select_echo_cancellation(enabled);
                }
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SpeechVoices(voices) => self.speech_voices = voices,
            Message::SpeechPrefs(prefs) => {
                speech::apply(&prefs);
                self.audio_preferences.speech = prefs;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SpeechTest => speech::speak("This is how messages will sound.".to_owned()),
            Message::StopSpeech => speech::stop(),
            Message::ToggleUnverifiedList => self.ui.unverified_open = !self.ui.unverified_open,
            Message::UnverifiedWrite(group, on) => {
                if let Some(session) = self.phase.session() {
                    session.client.set_write_group(group, on);
                }
            }
            Message::UnverifiedWriteAll(on) => {
                if let Some(session) = self.phase.session() {
                    unverified::set_all(&session.client, on);
                }
            }
            Message::ChatPrefs(prefs) => {
                self.audio_preferences.chat = prefs;
                chat_prefs::apply(prefs);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::InterfaceSoundsToggled(on) => {
                self.audio_preferences.interface_sounds = on;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::InterfaceVolumeChanged(volume) => {
                self.audio_preferences.interface_volume = volume;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::NotificationsToggled(on) => {
                self.audio_preferences.notifications_off = !on;
                notify::set_enabled(on);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::NotificationPreviewsToggled(on) => {
                self.audio_preferences.notification_previews_off = !on;
                notify::set_previews(on);
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::AudioPreferencesSaved(result) => {
                let saved = self.audio_saving.take();
                if let Err(error) = result {
                    self.notice = Some(error.to_owned());
                }
                if saved.as_ref() != Some(&self.audio_preferences) {
                    return self.save_audio_preferences();
                }
            }
            _ => {}
        }
        Task::none()
    }
}
