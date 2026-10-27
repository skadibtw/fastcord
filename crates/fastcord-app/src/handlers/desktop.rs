//! Window, tray, startup, shortcuts and the network page.

use crate::*;

impl App {
    pub(crate) fn desktop_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::NetworkProgress => {}
            Message::NetworkChanged => {
                self.network.controller().network_changed();
            }
            Message::Network(action) => return self.network_event(action),
            Message::Initialized(preferences, network, store) => {
                self.network =
                    network_ui::configure(&self.network, &network).unwrap_or_else(|_| {
                        self.network.with_mode(fastcord_net::settings::Mode::Proxy)
                    });
                self.network_settings.apply(&network);
                if let Some(warning) = network.warning {
                    self.notice = Some(warning.to_owned());
                }
                notify::set_enabled(!preferences.notifications_off);
                notify::set_previews(!preferences.notification_previews_off);
                self.ui.soundboard.set_prefs(preferences.soundboard.clone());
                self.audio_preferences = preferences;
                chat_prefs::apply(self.audio_preferences.chat);
                organize::publish(&self.audio_preferences.organize);
                speech::apply(&self.audio_preferences.speech);
                self.configure_ui_sound();
                self.shortcuts.rebuild(&self.audio_preferences.voice_input);
                // Fake mode signs in at once with its fixed token, memory-only.
                // `--login-screen` shows the sign-in screen with two pretend
                // saved accounts; Continue signs in with the fake token.
                #[cfg(feature = "fake-discord")]
                if fake::active() && fake::login_screen() {
                    self.phase = Phase::Login;
                    for (id, name) in [(1001, "Fake Me"), (1002, "Juno")] {
                        self.saved_accounts.push(Snowflake(id));
                        self.account_index.upsert(&fastcord_model::User {
                            id: Snowflake(id),
                            username: name.to_lowercase(),
                            global_name: Some(name.to_owned()),
                            avatar: None,
                            bot: false,
                        });
                    }
                    return Task::none();
                }
                #[cfg(feature = "fake-discord")]
                if fake::active() {
                    self.phase = Phase::Authenticating {
                        saved_account: None,
                    };
                    return Task::perform(
                        login::token_login(
                            self.network.clone(),
                            Arc::new(UserToken::new(fake::TOKEN.to_owned())),
                            None,
                            false,
                        ),
                        Message::LoginFinished,
                    );
                }
                return self.handle(Message::StoreReady(store));
            }
            Message::VoiceInput(event) => return self.voice_input_event(event),
            #[cfg(feature = "fake-discord")]
            Message::FakeStep(step) => return self.fake_step(step),
            #[cfg(feature = "fake-discord")]
            Message::FakeShot(path, shot) => fake::save(&path, &shot),
            Message::Tray(event) => match event {
                desktop::TrayEvent::Started => self.tray = Ok(()),
                desktop::TrayEvent::Failed(error) => self.tray = Err(error),
                desktop::TrayEvent::Command(command) => return self.tray_command(command),
            },
            Message::Forwarded(request) => {
                if let fastcord_platform::instance::Forwarded::Open(text) = request
                    && let Some(link) = desktop::parse(&text)
                {
                    self.pending_link = Some(link);
                }
                return self.show_window();
            }
            Message::CloseRequested => {
                match desktop::close_action(self.audio_preferences.close_to_tray, self.tray.is_ok())
                {
                    desktop::CloseAction::Hide => {
                        self.window_hidden = true;
                        self.window_drawable = false;
                        self.sync_viewing();
                        return iced::window::latest()
                            .and_then(|id| iced::window::set_mode(id, iced::window::Mode::Hidden));
                    }
                    desktop::CloseAction::Quit => return self.handle(Message::Quit),
                }
            }
            Message::Quit => {
                let volume_writer = self.volume_writer.take();
                // Leaving the call first lets the server drop the voice state at
                // once. The Gateway then closes with 1000 (waiting at most 2 s
                // for its task) so the session ends instead of lingering until a
                // heartbeat times out; the process exit releases the rest.
                let in_call = self.call_active();
                let controls = match &self.phase {
                    Phase::Account { gateway, .. } => Some(gateway.controls.clone()),
                    _ => None,
                };
                if !in_call && controls.is_none() && volume_writer.is_none() {
                    return iced::exit();
                }
                let leave = if in_call {
                    self.handle(Message::LeaveVoice)
                } else {
                    Task::none()
                };
                return Task::batch([
                    leave,
                    Task::perform(
                        async move {
                            if let Some(writer) = volume_writer {
                                let _ = tokio::task::spawn_blocking(move || writer.finish()).await;
                            }
                            if in_call {
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            }
                            if let Some(controls) = controls {
                                controls.close_gateway(QUIT_CLOSE_WAIT).await;
                            }
                        },
                        |()| Message::Exit,
                    ),
                ]);
            }
            Message::Exit => return iced::exit(),
            Message::ReconnectGateway => {
                let stopped = match &self.phase {
                    Phase::Account {
                        session,
                        persistence,
                        gateway,
                    } => matches!(
                        &gateway.status,
                        GatewayStatus::Stopped(reason) if reason.allows_manual_reconnect()
                    )
                    .then(|| (Arc::clone(session), *persistence)),
                    _ => None,
                };
                if let Some((session, persistence)) = stopped {
                    // Drafts are the user's, not the worker's: the composer
                    // moves to the new panel (#149).
                    let composer = match &mut self.phase {
                        Phase::Account { gateway, .. } => std::mem::take(&mut gateway.composer),
                        _ => composer::Composer::default(),
                    };
                    // The user asked: the new Gateway task gets fresh
                    // reconnect attempts (#139).
                    session.client.allow_manual_gateway_reconnect();
                    let old = match &self.phase {
                        Phase::Account { gateway, .. } => Some(gateway.controls.clone()),
                        _ => None,
                    };
                    let task = self.enter_account_from(session, persistence, old);
                    if let Some(gateway) = self.phase.gateway_mut() {
                        gateway.composer = composer;
                    }
                    return task;
                }
            }
            Message::CloseToTrayToggled(on) => {
                self.audio_preferences.close_to_tray = on;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::AutostartToggled(on) => {
                if !self.autostart_busy {
                    self.autostart_busy = true;
                    return desktop::autostart_task(Some(on));
                }
            }
            Message::AutostartDone(result) => {
                self.autostart_busy = false;
                match result {
                    Ok(on) => self.autostart = Some(on),
                    Err(error) => self.notice = Some(error),
                }
            }
            Message::ToggleCompact => {
                if self.compact_call {
                    return self.end_compact();
                }
                if self.call_active() {
                    self.compact_call = true;
                    let leave_fullscreen = std::mem::take(&mut self.window_fullscreen);
                    return iced::window::latest().and_then(move |id| {
                        if leave_fullscreen {
                            iced::window::set_mode(id, iced::window::Mode::Windowed)
                        } else {
                            Task::none()
                        }
                        .chain(iced::window::set_level(
                            id,
                            iced::window::Level::AlwaysOnTop,
                        ))
                        .chain(iced::window::resize(id, desktop::COMPACT_SIZE))
                    });
                }
            }
            Message::ShortcutKey(input) => {
                if input.down && input.code == "Escape" && self.shortcuts.recording.is_some() {
                    let cancelled = self.shortcuts.cancel_record().into_iter().collect();
                    return self.apply_shortcuts(cancelled);
                }
                let scope = self.audio_preferences.voice_input.scope;
                let actions = self.shortcuts.window_key(scope, &input);
                return self.apply_shortcuts(actions);
            }
            Message::GlobalShortcut(event) => {
                use shortcuts::GlobalEvent;
                match event {
                    GlobalEvent::Started => {
                        self.shortcuts.global_running = true;
                        self.shortcuts.global_error = None;
                    }
                    GlobalEvent::Failed(error) => {
                        self.shortcuts.global_running = false;
                        self.shortcuts.global_error = Some(error);
                    }
                    GlobalEvent::Lost => self.release_held_shortcuts(),
                    GlobalEvent::Event(event) => {
                        if let Some(action) = self.shortcuts.global(event) {
                            return self.apply_shortcuts(vec![action]);
                        }
                    }
                }
            }
            Message::SetWindowFullscreen(on) => {
                self.window_fullscreen = on;
                return iced::window::latest().and_then(move |id| {
                    iced::window::set_mode(
                        id,
                        if on {
                            iced::window::Mode::Fullscreen
                        } else {
                            iced::window::Mode::Windowed
                        },
                    )
                });
            }
            Message::WindowActive(active) => {
                self.window_active = active;
                self.sync_viewing();
                notify::set_active(active);
                if !active {
                    let scope = self.audio_preferences.voice_input.scope;
                    let released = self.shortcuts.window_lost_focus(scope);
                    return self.apply_shortcuts(released);
                }
            }
            Message::WindowSize(size) => {
                self.window_drawable = size.width > 0.0 && size.height > 0.0;
                if self.window_drawable && !self.compact_call {
                    self.window_size = size;
                }
                self.sync_viewing();
            }
            _ => {}
        }
        Task::none()
    }
}
