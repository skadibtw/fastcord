//! The panels' event routers: each panel's event reaches its state here with
//! the account's session and panel, and hand-offs between panels are made
//! (#169).

use crate::*;

impl App {
    /// Discord rejected the login: stop the account like any other 401.
    fn reject_login(&mut self, id: u64) -> Task<Message> {
        self.handle(Message::Gateway(
            id,
            gateway::GatewayStatus::AuthenticationRequired,
        ))
    }

    pub(crate) fn forum_event(&mut self, event: forum::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let (task, opened) =
            self.ui
                .forum
                .update(event, &session.client, gateway.navigation.forum.as_ref());
        if let Some(thread) = opened
            && let Some(guild) = thread.guild_id
        {
            gateway.controls.open_thread(guild, thread);
        }
        task.map(Message::Forum)
    }

    pub(crate) fn threads_event(&mut self, event: threads::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        match event {
            threads::Event::OpenArchived(index) => {
                if let Some((guild, thread)) = self.ui.threads.archived_thread(index) {
                    self.ui.threads.close();
                    gateway.controls.open_thread(guild, thread);
                }
                Task::none()
            }
            threads::Event::SetState {
                thread,
                archived,
                locked,
            } => {
                // Checked against the current read model right before sending.
                let allowed = gateway.navigation.selection.as_ref().is_some_and(|s| {
                    s.channel_id == thread
                        && s.permissions.manage_threads
                        && s.thread.is_some_and(|t| {
                            archived.is_none_or(|a| a != t.archived)
                                && locked.is_none_or(|l| l != t.locked)
                        })
                });
                self.ui
                    .threads
                    .set_state((thread, archived, locked), allowed, &session.client)
                    .map(Message::Threads)
            }
            threads::Event::Created(Ok(thread)) => {
                self.ui.threads.close();
                if let Some(guild) = thread.guild_id {
                    gateway.controls.open_thread(guild, *thread);
                }
                Task::none()
            }
            event => self
                .ui
                .threads
                .update(event, &session.client)
                .map(Message::Threads),
        }
    }

    pub(crate) fn gif_event(&mut self, event: gif::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account_mut() else {
            return Task::none();
        };
        if let gif::Event::Pick(index) = event {
            if let Some(url) = self.ui.gif.url(index).map(str::to_owned)
                && gateway.composer.insert_emoji(&url)
            {
                self.ui.gif.close();
                return iced::widget::operation::focus(composer::EDITOR_ID);
            }
            return Task::none();
        }
        if matches!(event, gif::Event::Toggle) && !self.ui.gif.open && !gateway.timeline.can_send {
            return Task::none();
        }
        self.ui
            .gif
            .update(event, &session.client, gateway.attachments.as_ref())
            .map(Message::Gif)
    }

    pub(crate) fn assets_event(&mut self, event: assets::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = assets::State::is_rejected(&event);
        let env = assets::Env {
            client: &session.client,
            navigation: &gateway.navigation,
            cache: gateway.attachments.as_ref(),
            output: self.audio_preferences.output.choice.clone(),
        };
        let task = self.ui.assets.update(event, &env);
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Assets)
    }

    /// Lists stickers and sounds again when Discord reports a change.
    pub(crate) fn assets_follow(&mut self) -> Task<Message> {
        let Some(gateway) = self.phase.gateway() else {
            return Task::none();
        };
        let (stickers, sounds) = gateway
            .controls
            .voice_controls_with(|c| (c.sticker_change, c.sound_catalog_change));
        let tasks: Vec<_> = self
            .ui
            .assets
            .new_changes(stickers, sounds)
            .into_iter()
            .map(|event| self.assets_event(event))
            .collect();
        Task::batch(tasks)
    }

    pub(crate) fn apps_event(&mut self, event: apps::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = apps::State::is_rejected(&event);
        let (task, action) = self
            .ui
            .apps
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            return self.reject_login(gateway.id);
        }
        let task = task.map(Message::Apps);
        match action {
            Some(apps::Action::Open(url)) => {
                open_link(&url);
                task
            }
            Some(apps::Action::Copy(text)) => Task::batch([task, iced::clipboard::write(text)]),
            None => task,
        }
    }

    pub(crate) fn announce_event(&mut self, event: announce::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = announce::State::is_rejected(&event);
        let task = self.ui.announce.update(
            event,
            &gateway.navigation,
            &gateway.timeline,
            &session.client,
        );
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Announce)
    }

    pub(crate) fn forward_event(&mut self, event: forward::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let candidates = forward_candidates(gateway);
        self.ui
            .forward
            .update(event, &candidates, &session.client)
            .map(Message::Forward)
    }

    /// The scope whose application commands the open conversation can use.
    pub(crate) fn slash_scope(gateway: &gateway::GatewayPanel, channel: Snowflake) -> IndexScope {
        match gateway.navigation.selected_guild {
            Some(guild) if gateway.navigation.selection.is_some() => IndexScope::Guild(guild),
            _ => IndexScope::Channel(channel),
        }
    }

    /// The draft changed: a leading slash loads the command list.
    pub(crate) fn slash_typed(&mut self) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let env = slash::Env {
            channel,
            scope: Self::slash_scope(gateway, channel),
            can_send: gateway.timeline.can_send,
            client: &session.client,
            bridge: &gateway.controls,
            interaction: &gateway.timeline.interaction,
            now: std::time::Instant::now(),
        };
        self.ui
            .slash
            .typed(gateway.composer.slash_query().is_some(), &env)
            .map(Message::Slash)
    }

    pub(crate) fn slash_event(&mut self, event: slash::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account_mut() else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let env = slash::Env {
            channel,
            scope: Self::slash_scope(gateway, channel),
            can_send: gateway.timeline.can_send,
            client: &session.client,
            bridge: &gateway.controls,
            interaction: &gateway.timeline.interaction,
            now: std::time::Instant::now(),
        };
        let rejected = slash::State::is_rejected(&event);
        let (task, used) = self.ui.slash.update(event, &env);
        if used {
            gateway.composer.clear_text();
        }
        if rejected {
            // Discord rejected the login: stop the account like any other 401.
            let id = gateway.id;
            return self.reject_login(id);
        }
        task.map(Message::Slash)
    }

    pub(crate) fn poll_event(&mut self, event: poll_create::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let context = poll_create::Context {
            channel,
            can_send: gateway.timeline.can_send,
        };
        self.ui
            .poll
            .update(event, context, &session.client)
            .map(Message::Poll)
    }

    /// The voice-message panel. A recording the user sent goes through the ordinary outbox as a
    /// text-free message with one file, so its upload, progress, cancel, retry and ambiguity
    /// handling are those of any attachment.
    pub(crate) fn voice_event(&mut self, event: voice_record::Event) -> Task<Message> {
        let Some(gateway) = self.phase.gateway() else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let context = self.voice_context(gateway, channel);
        let (task, file) = self.ui.voice.update(event, &context);
        if let Some(file) = file {
            if gateway.controls.send_message_with_files(
                channel,
                String::new(),
                vec![file],
                false,
                None,
            ) {
                self.ui.voice.sent();
            } else {
                self.notice = Some("The message could not be queued. Try again.".to_owned());
            }
        }
        task.map(Message::Voice)
    }

    pub(crate) fn voice_context(
        &self,
        gateway: &gateway::GatewayPanel,
        channel: Snowflake,
    ) -> voice_record::Context {
        voice_record::Context {
            channel,
            can_send: gateway.timeline.can_send,
            enabled: self.audio_preferences.voice_recording,
            input: self.audio_preferences.input.choice.clone(),
            output: self.audio_preferences.output.choice.clone(),
            noise_suppression: self.audio_preferences.noise_suppression,
        }
    }

    /// Microphone gain and checks in the audio settings. A gain change applies to the call, a
    /// running check and the saved preference at once; the file is written when the slider is
    /// released.
    pub(crate) fn mic_event(&mut self, event: mic_panel::Event) -> Task<Message> {
        use mic_panel::Event;
        let gain = self.audio_preferences.input_gain;
        let changed = match event {
            Event::Gain(db) => Some((
                fastcord_audio::InputGain {
                    manual_db: db,
                    ..gain
                }
                .clamped(),
                false,
            )),
            Event::Automatic(on) => Some((
                fastcord_audio::InputGain {
                    automatic: on,
                    ..gain
                },
                true,
            )),
            Event::GainReleased => {
                return if self.audio_saving.is_none() {
                    self.save_audio_preferences()
                } else {
                    Task::none()
                };
            }
            Event::Tick => {
                let call = match &self.phase {
                    Phase::Account { gateway, .. } => {
                        gateway.controls.voice_controls().input_diagnostics
                    }
                    _ => None,
                };
                self.mic_panel.tick(call);
                return Task::none();
            }
            _ => None,
        };
        if let Some((gain, save)) = changed {
            self.audio_preferences.input_gain = gain;
            if let Some(gateway) = self.phase.gateway() {
                gateway.controls.select_input_gain(gain);
            }
            self.mic_panel.set_gain(gain);
            return if save && self.audio_saving.is_none() {
                self.save_audio_preferences()
            } else {
                Task::none()
            };
        }
        let context = mic_panel::Context {
            input: self.audio_preferences.input.choice.clone(),
            output: self.audio_preferences.output.choice.clone(),
            noise_suppression: self.audio_preferences.noise_suppression,
            gain,
            in_call: self.call_active(),
        };
        self.mic_panel
            .update(event, &context)
            .map(Message::MicPanel)
    }

    /// The environment a soundboard update borrows: the account, the call and the output.
    pub(crate) fn soundboard_env(
        phase: &Phase,
        output: fastcord_audio::DeviceChoice,
    ) -> Option<soundboard::Env<'_>> {
        let Phase::Account {
            session, gateway, ..
        } = phase
        else {
            return None;
        };
        let deafened = gateway
            .controls
            .voice_controls_with(|c| c.deafened || c.server_deafened);
        let in_call = gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Idle;
        Some(soundboard::Env {
            client: &session.client,
            cache: gateway.attachments.as_ref(),
            output,
            deafened,
            voice: gateway.controls.voice_target().filter(|_| in_call),
        })
    }

    /// Saves the soundboard choices when an update changed them.
    pub(crate) fn save_soundboard_prefs(&mut self) -> Task<Message> {
        match self.ui.soundboard.take_dirty_prefs() {
            Some(prefs) => {
                self.audio_preferences.soundboard = prefs;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
                Task::none()
            }
            None => Task::none(),
        }
    }

    /// Follows the call after each gateway notification: sounds end with the call or a
    /// deafen, the panel follows its channel, and new plays and catalog changes arrive.
    pub(crate) fn soundboard_follow(&mut self) -> Task<Message> {
        let output = self.audio_preferences.output.choice.clone();
        let Some(env) = Self::soundboard_env(&self.phase, output) else {
            return Task::none();
        };
        let Some(gateway) = self.phase.gateway() else {
            return Task::none();
        };
        let (cues, catalog_change) = gateway
            .controls
            .voice_controls_with(|c| (c.sound_cues.clone(), c.sound_catalog_change));
        self.ui.soundboard.sync_call(env.voice, env.deafened);
        let mut tasks = Vec::new();
        for cue in &cues {
            tasks.push(
                self.ui
                    .soundboard
                    .update(soundboard::Event::Cue(*cue), &env),
            );
        }
        if let Some(guild) = self.ui.soundboard.new_catalog_change(catalog_change) {
            tasks.push(
                self.ui
                    .soundboard
                    .update(soundboard::Event::CatalogChanged(guild), &env),
            );
        }
        let saved = self.save_soundboard_prefs();
        Task::batch(
            tasks
                .into_iter()
                .map(|task| task.map(Message::Soundboard))
                .chain([saved]),
        )
    }

    pub(crate) fn stage_event(&mut self, event: stage::Event) -> Task<Message> {
        use stage::Event;
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let stage = gateway.controls.voice_controls().stage;
        let action = match event {
            Event::Done(serial, result) => {
                self.ui.stage.finish(serial, result);
                return Task::none();
            }
            Event::OpenTopic(editing) => {
                let draft = if editing {
                    stage.topic.clone().unwrap_or_default()
                } else {
                    String::new()
                };
                self.ui.stage.topic = Some((editing, draft));
                return Task::none();
            }
            Event::Topic(value) => {
                if let Some((_, draft)) = &mut self.ui.stage.topic {
                    *draft = value
                        .chars()
                        .take(fastcord_discord::MAX_TOPIC_CHARS)
                        .collect();
                }
                return Task::none();
            }
            Event::CloseTopic => {
                self.ui.stage.topic = None;
                return Task::none();
            }
            Event::SubmitTopic => match self.ui.stage.submission() {
                Some(action) => action,
                None => return Task::none(),
            },
            Event::Do(action) => action,
        };
        self.ui.member_menu.close();
        let (Some(guild), Some(channel)) = (stage.guild, stage.channel) else {
            return Task::none();
        };
        if session.client.writes_restricted() {
            return Task::none();
        }
        let Some(serial) = self.ui.stage.begin() else {
            return Task::none();
        };
        let bridge = gateway.controls.clone();
        Task::perform(
            async move { bridge.stage(guild, channel, action).await },
            move |result| Message::Stage(Event::Done(serial, result)),
        )
    }

    pub(crate) fn soundboard_event(&mut self, event: soundboard::Event) -> Task<Message> {
        let output = self.audio_preferences.output.choice.clone();
        let Some(env) = Self::soundboard_env(&self.phase, output) else {
            return Task::none();
        };
        let rejected = soundboard::State::is_rejected(&event);
        let task = self.ui.soundboard.update(event, &env);
        if rejected && let Some(gateway) = self.phase.gateway() {
            // Discord rejected the login: stop the account like any other 401.
            return self.reject_login(gateway.id);
        }
        let saved = self.save_soundboard_prefs();
        Task::batch([task.map(Message::Soundboard), saved])
    }

    pub(crate) fn sticker_event(&mut self, event: sticker::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        let context = sticker::Context {
            guild: gateway
                .navigation
                .selected_guild
                .filter(|_| !gateway.directs),
            channel,
            can_send: gateway.timeline.can_send,
        };
        self.ui
            .sticker
            .update(
                event,
                context,
                &session.client,
                gateway.attachments.as_ref(),
            )
            .map(Message::Sticker)
    }

    pub(crate) fn group_event(&mut self, event: group_dm::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = group_dm::State::is_rejected(&event);
        let task =
            self.ui
                .group
                .update(event, &gateway.navigation, session.user.id, &session.client);
        if rejected {
            // Discord rejected the login: stop the account like any other 401.
            return self.reject_login(gateway.id);
        }
        task.map(Message::Group)
    }

    pub(crate) fn onboarding_event(&mut self, event: onboarding::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        match event {
            onboarding::Event::Rich(event) => return self.handle(Message::Timeline(event)),
            onboarding::Event::GoChannel(guild, channel) => {
                // The guide never opens a channel the account cannot see.
                if onboarding::visible_channel(&gateway.navigation, channel) {
                    gateway.controls.select_channel(guild, channel);
                }
                return Task::none();
            }
            _ => {}
        }
        let rejected = onboarding::State::is_rejected(&event);
        let task = self
            .ui
            .onboarding
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Onboarding)
    }

    pub(crate) fn events_event(&mut self, event: scheduled_events::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        if let scheduled_events::Event::Join(guild, channel) = event {
            // Only an event that is still held and not over, in a channel the
            // account may connect to, joins; the join path checks the channel again.
            if scheduled_events::joinable(&gateway.navigation, guild, channel) {
                return self.handle(Message::JoinVoice(guild, channel));
            }
            return Task::none();
        }
        let rejected = scheduled_events::State::is_rejected(&event);
        let task = self.ui.events.update(
            event,
            &gateway.navigation,
            &session.client,
            &gateway.controls,
        );
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Events)
    }

    pub(crate) fn discover_event(&mut self, event: discover::Event) -> Task<Message> {
        let Some(session) = self.phase.session() else {
            return Task::none();
        };
        let rejected = discover::State::is_rejected(&event);
        let task = self.ui.discover.update(event, &session.client);
        if rejected && let Some(gateway) = self.phase.gateway() {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Discover)
    }

    pub(crate) fn invite_event(&mut self, event: invite::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = invite::State::is_rejected(&event);
        let task = self.ui.invite.update(
            event,
            &gateway.navigation,
            &gateway.timeline.rows,
            &session.client,
        );
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Invite)
    }

    pub(crate) fn community_event(&mut self, event: community::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = community::State::is_rejected(&event);
        let task =
            self.ui
                .community
                .update(event, &gateway.navigation, &session.client, session.user.id);
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Community)
    }

    pub(crate) fn guild_admin_event(&mut self, event: guild_admin::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        match event {
            // The rail's right click opens the same menu as the server's name.
            guild_admin::Event::Menu(guild) => {
                self.close_menus();
                self.ui.menu = Some(Menu::Server(guild));
                return Task::none();
            }
            guild_admin::Event::Apps(guild) => {
                return self.apps_event(apps::Event::OpenGuild(guild));
            }
            guild_admin::Event::Webhooks(guild, channel) => {
                return self.apps_event(apps::Event::OpenWebhooks(guild, channel));
            }
            _ => {}
        }
        if let guild_admin::Event::Assets(guild) = event {
            // The menu hands over to the emoji, sticker and sound panel.
            let _ = self
                .ui
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.assets_event(assets::Event::Open(guild));
        }
        if let guild_admin::Event::Followed(guild, channel) = event {
            // The channel panel hands over to the followed-announcements list.
            return self.announce_event(announce::Event::OpenFollowers(guild, channel));
        }
        if let guild_admin::Event::Community(guild) = event {
            // The settings hand over to the community setup panel.
            let _ = self.ui.guild_admin.update(
                guild_admin::Event::Close,
                &gateway.navigation,
                &session.client,
            );
            return self.community_event(community::Event::Open(guild));
        }
        if let guild_admin::Event::Leave(guild) = event {
            // The menu hands over to the leave confirmation of the invite module.
            let _ = self
                .ui
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.invite_event(invite::Event::AskLeave(guild));
        }
        if let guild_admin::Event::AutoMod(guild) = event {
            // The menu hands over to the AutoMod panel.
            let _ = self
                .ui
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.automod_event(automod::Event::Open(guild));
        }
        if let guild_admin::Event::Bans(guild) | guild_admin::Event::Audit(guild) = event {
            // The menu hands over to the moderation panels.
            let audit = matches!(event, guild_admin::Event::Audit(_));
            let _ = self
                .ui
                .guild_admin
                .update(event, &gateway.navigation, &session.client);
            return self.moderation_event(if audit {
                moderation::Event::Audit(guild)
            } else {
                moderation::Event::Bans(guild)
            });
        }
        let rejected = guild_admin::State::is_rejected(&event);
        let task = self
            .ui
            .guild_admin
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::GuildAdmin)
    }

    pub(crate) fn moderation_event(&mut self, event: moderation::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = moderation::State::is_rejected(&event);
        let task = self
            .ui
            .moderation
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::Moderation)
    }

    pub(crate) fn automod_event(&mut self, event: automod::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = automod::State::is_rejected(&event);
        let task = self
            .ui
            .automod
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            return self.reject_login(gateway.id);
        }
        task.map(Message::AutoMod)
    }

    pub(crate) fn identity_event(&mut self, event: profile_edit::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = profile_edit::State::is_rejected(&event);
        let task = self.ui.identity.update(
            event,
            &gateway.navigation,
            own_user(gateway, session),
            &session.client,
        );
        if rejected {
            let id = gateway.id;
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Identity)
    }

    pub(crate) fn member_menu_event(&mut self, event: member_menu::Event) -> Task<Message> {
        use member_menu::Event;
        let Some(gateway) = self.phase.gateway() else {
            return Task::none();
        };
        match event {
            Event::Open(user, point) => {
                self.ui.member_menu.target = Some(member_menu::Target {
                    user,
                    point,
                    guild: (!gateway.directs)
                        .then_some(gateway.navigation.selected_guild)
                        .flatten(),
                    confirm: None,
                    sub: None,
                    reason: String::new(),
                    nick: String::new(),
                    ban_window: 0,
                });
                self.ui.member_menu.notice = None;
            }
            Event::Close => {
                self.ui.member_menu.close();
                if let Some(writer) = &self.volume_writer {
                    writer.flush();
                }
            }
            Event::Done(serial, result) => {
                self.ui.member_menu.done(
                    serial,
                    result.map_or_else(moderation::failure, |()| "Done.".into()),
                );
            }
            Event::Sub(sub) => {
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.sub = sub;
                    target.confirm = None;
                    if sub == Some(member_menu::Sub::Nick) {
                        // Start from the nickname the member shows now.
                        target.nick = gateway
                            .navigation
                            .members
                            .get(&target.user.id)
                            .and_then(|m| m.nick.clone())
                            .unwrap_or_default();
                    }
                }
            }
            Event::Reason(text) => {
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.reason = fastcord_discord::cut_reason(text);
                }
            }
            Event::Nick(text) => {
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.nick = text
                        .chars()
                        .take(fastcord_discord::MAX_NICK_CHARS)
                        .collect();
                }
            }
            Event::BanWindow(seconds) => {
                if let Some(target) = &mut self.ui.member_menu.target {
                    target.ban_window = seconds;
                }
            }
            Event::Select(action) => {
                if self.ui.member_menu.pending.is_some() {
                    return Task::none();
                }
                let Some(target) = &mut self.ui.member_menu.target else {
                    return Task::none();
                };
                if action.needs_confirm() {
                    target.confirm = Some(action);
                    return Task::none();
                }
                return self.begin_member_action(action);
            }
            Event::Confirm => {
                if let Some(action) = self
                    .ui
                    .member_menu
                    .target
                    .as_ref()
                    .and_then(|target| target.confirm.clone())
                {
                    return self.begin_member_action(action);
                }
            }
        }
        Task::none()
    }

    pub(crate) fn begin_member_action(
        &mut self,
        action: fastcord_discord::MemberAction,
    ) -> Task<Message> {
        use fastcord_discord::MemberAction;
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let Some(target) = &self.ui.member_menu.target else {
            return Task::none();
        };
        let Some(guild) = target
            .guild
            .filter(|id| Some(*id) == gateway.navigation.selected_guild)
        else {
            return Task::none();
        };
        if self.ui.member_menu.pending.is_some() || session.client.writes_restricted() {
            return Task::none();
        }
        let own = gateway
            .navigation
            .current_user
            .as_ref()
            .is_some_and(|me| me.id == target.user.id);
        let allowed = gateway
            .navigation
            .members
            .get(&target.user.id)
            .is_some_and(|member| member.moderation.allows(&action))
            && match &action {
                MemberAction::OwnNick(_) => own,
                MemberAction::Move(dest) => gateway
                    .navigation
                    .move_targets
                    .iter()
                    .any(|(id, _)| id == dest),
                _ => !own,
            };
        if !allowed {
            self.ui.member_menu.notice =
                Some("This action is no longer available for this member.".into());
            return Task::none();
        }
        let reason = Some(target.reason.trim().to_owned()).filter(|r| !r.is_empty());
        let (bridge, user) = (gateway.controls.clone(), target.user.id);
        let serial = self.ui.member_menu.begin(user);
        Task::perform(
            async move { bridge.moderate(guild, user, action, reason).await },
            move |result| Message::MemberMenu(member_menu::Event::Done(serial, result)),
        )
    }

    pub(crate) fn report_event(&mut self, event: report::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        // Ignore and Block after a report are their own requests.
        let follow = match &event {
            report::Event::Follow(action, user) => Some((*action, *user)),
            _ => None,
        };
        if matches!(event, report::Event::OpenServer) {
            let Some(guild) = gateway
                .navigation
                .selected_guild
                .filter(|_| !gateway.directs)
            else {
                return Task::none();
            };
            let subject = report::Subject::new(
                fastcord_discord::ReportTarget::Server(guild),
                "server",
                &gateway.navigation.selected_guild_name,
            );
            return self
                .ui
                .report
                .begin(subject, &session.client)
                .map(Message::Report);
        }
        // The review is judged against what the timeline holds right now.
        self.ui
            .report
            .sync(self.ui.report.watched().and_then(|(channel, message)| {
                timeline::is_retained(&gateway.timeline, channel, message)
            }));
        let rejected = report::State::is_rejected(&event);
        let mut task = self
            .ui
            .report
            .update(event, &session.client)
            .map(Message::Report);
        if let Some((action, user)) = follow {
            task = Task::batch([
                task,
                self.ui
                    .friends
                    .update(friends::Event::Act(user, action), &session.client)
                    .map(Message::Friends),
            ]);
        }
        if rejected {
            let id = gateway.id;
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task
    }

    pub(crate) fn safety_event(&mut self, event: safety::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = safety::State::is_rejected(&event);
        let task = self
            .ui
            .safety
            .update(event, &gateway.navigation, &session.client);
        if rejected {
            let id = gateway.id;
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Safety)
    }

    /// Events of the quick switcher and the shortcuts help.
    pub(crate) fn switcher_event(&mut self, event: switcher::Event) -> Task<Message> {
        // Enter on a pasted Discord link opens it instead of a search result.
        if matches!(event, switcher::Event::Submit)
            && let Some(link) = self.ui.switcher.wanted().and_then(desktop::parse)
        {
            self.ui.switcher.close();
            return self.open_link(link);
        }
        let Some(gateway) = self.phase.gateway_mut() else {
            return Task::none();
        };
        match self.ui.switcher.update(event, &gateway.navigation.quick) {
            switcher::Action::None => Task::none(),
            switcher::Action::Open(entry) => {
                // Exactly a sidebar click: the conversation opens (a voice
                // channel's chat, never its call) and its draft comes back.
                self.ui.threads.close();
                self.ui.friends.close();
                if self.ui.inbox.is_open() {
                    self.ui.inbox.clear();
                    gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
                }
                gateway.directs = entry.guild.is_none();
                gateway.controls.open_channel(entry.channel);
                self.ui.nav_pending = true;
                iced::widget::operation::focus(composer::EDITOR_ID)
            }
        }
    }

    /// A navigation chord (see [`switcher`]).
    pub(crate) fn navigation_shortcut(&mut self, shortcut: switcher::Shortcut) -> Task<Message> {
        use switcher::Shortcut;
        if let Shortcut::Escape { .. } = shortcut {
            // Whatever is on top closes first: a popover, a dialog, the
            // expressions window, a settings screen, then the side panels.
            if self.ui.status.open || self.mic_menu || self.ui.menu.is_some() {
                self.close_menus();
                return Task::none();
            }
            if self.ui.new_dm.open {
                self.ui.new_dm.update(new_dm::Event::Close);
                return Task::none();
            }
            if let Some(close) = self.dialog_close() {
                return close.map_or_else(Task::none, |close| self.handle(close));
            }
            if let Some(gateway) = self.phase.gateway()
                && self.expressions_tab(gateway).is_some()
            {
                self.close_expressions();
                return iced::widget::operation::focus(composer::EDITOR_ID);
            }
            if self.server_page().is_some() {
                return self.close_server_pages();
            }
            if self.audio_settings.open {
                return self.handle(Message::Settings(settings::Event::Close));
            }
        }
        let Some(gateway) = self.phase.gateway_mut() else {
            return Task::none();
        };
        let composer = || iced::widget::operation::focus(composer::EDITOR_ID);
        // While a voice binding is being recorded the chords belong to it.
        if self.shortcuts.recording.is_some() && !matches!(shortcut, Shortcut::Escape { .. }) {
            return Task::none();
        }
        match shortcut {
            Shortcut::Switcher => {
                self.ui.switcher.toggle();
                if self.ui.switcher.is_open() {
                    iced::widget::operation::focus(switcher::INPUT_ID)
                } else {
                    composer()
                }
            }
            Shortcut::Help => {
                self.ui.switcher.toggle_help();
                composer()
            }
            Shortcut::Move(delta) => {
                if self.ui.switcher.is_open() {
                    self.ui
                        .switcher
                        .update(switcher::Event::Move(delta), &gateway.navigation.quick);
                }
                Task::none()
            }
            Shortcut::Step(step) => {
                if self.ui.switcher.is_open() || self.ui.switcher.help_open() {
                    return Task::none();
                }
                self.ui.threads.close();
                self.ui.friends.close();
                if self.ui.inbox.is_open() {
                    self.ui.inbox.clear();
                    gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
                }
                if step.scope == fastcord_discord::state::navigation::StepScope::Server {
                    gateway.directs = false;
                }
                gateway.controls.step(step);
                self.ui.nav_pending = true;
                Task::none()
            }
            Shortcut::Escape { captured } => {
                // Then the side panels, the topmost first.
                if self.ui.inbox.is_open() {
                    return Task::done(Message::Inbox(inbox::Event::Toggle));
                }
                if self.ui.notify_settings.is_open() {
                    return Task::done(Message::NotifySettings(notify_settings::Event::Toggle));
                }
                if self.ui.search.open {
                    return Task::done(Message::Search(search::Event::Toggle));
                }
                if gateway
                    .timeline
                    .channel_id
                    .is_some_and(|channel| self.ui.pins.shows(channel))
                {
                    return Task::done(Message::Pins(pins::Event::Toggle));
                }
                if self.ui.member_menu.target.is_some() {
                    self.ui.member_menu.close();
                    return Task::none();
                }
                if self.ui.switcher.close() {
                    return composer();
                }
                if gateway.attachment_viewer.take().is_some() {
                    return composer();
                }
                if gateway.emoji_picker.request.open {
                    gateway.emoji_picker.close();
                    return composer();
                }
                // A focused composer handled Escape itself.
                if !captured {
                    if gateway.composer.editing().is_some() {
                        gateway.composer.cancel_edit();
                        return composer();
                    }
                    if gateway.composer.replying().is_some() {
                        gateway.composer.cancel_reply();
                        return composer();
                    }
                }
                Task::none()
            }
        }
    }

    /// A click on a folder, category or the hide-muted choice: kept locally for
    /// this account and applied by the next navigation snapshot.
    pub(crate) fn organize_event(&mut self, event: organize::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        self.audio_preferences
            .organize
            .apply(session.user.id.0, event);
        organize::publish(&self.audio_preferences.organize);
        gateway.controls.layout_changed();
        if self.audio_saving.is_none() {
            return self.save_audio_preferences();
        }
        Task::none()
    }

    pub(crate) fn notify_settings_event(&mut self, event: notify_settings::Event) -> Task<Message> {
        if let notify_settings::Event::HideMuted(guild) = event {
            return self.organize_event(organize::Event::HideMuted(guild));
        }
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rejected = matches!(
            &event,
            notify_settings::Event::Done(Err(RestError::AuthenticationRequired))
        );
        let task = self.ui.notify_settings.update(
            event,
            &gateway.navigation.notifications,
            &session.client,
        );
        if rejected {
            return self.handle(Message::Gateway(
                gateway.id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::NotifySettings)
    }

    pub(crate) fn inbox_event(&mut self, event: inbox::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let rows = &gateway.navigation.inbox.rows;
        match event {
            inbox::Event::MarkRead(channel) => {
                if let Some(row) = rows.iter().find(|row| row.channel == channel) {
                    gateway.controls.mark_read(row.guild, row.channel);
                }
                return Task::none();
            }
            inbox::Event::MarkAllRead(shown) => {
                gateway.controls.mark_read_many(shown);
                return Task::none();
            }
            _ => {}
        }
        let rejected = matches!(
            &event,
            inbox::Event::Loaded(_, Err(RestError::AuthenticationRequired))
                | inbox::Event::Muted(_, Err(RestError::AuthenticationRequired))
        );
        let (task, action) =
            self.ui
                .inbox
                .update(event, &gateway.navigation.inbox, &session.client);
        gateway.controls.inbox_rows(self.ui.inbox.rows_wanted());
        if let inbox::Action::Open(channel, target) = action {
            if gateway.timeline.channel_id == Some(channel) {
                inbox_jump(&gateway.controls, channel, target);
            } else {
                // Only a conversation the account can open is selected;
                // otherwise the wait ends with a notice.
                self.ui.inbox.set_pending(channel, target);
                gateway.controls.open_channel(channel);
            }
        }
        if rejected {
            // Discord rejected the login: stop the account like any other 401.
            return self.handle(Message::Gateway(
                gateway.id,
                gateway::GatewayStatus::AuthenticationRequired,
            ));
        }
        task.map(Message::Inbox)
    }

    pub(crate) fn pins_event(&mut self, event: pins::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        let Some(channel) = gateway.timeline.channel_id else {
            return Task::none();
        };
        if let pins::Event::Jump(index) = event {
            if self.ui.pins.shows(channel)
                && let Some(pin) = self.ui.pins.pin(index)
            {
                gateway.controls.timeline_intent(history::Intent::JumpTo {
                    channel,
                    message: pin.message,
                });
            }
            return Task::none();
        }
        self.ui
            .pins
            .update(event, &session.client, channel)
            .map(Message::Pins)
    }

    pub(crate) fn search_event(&mut self, event: search::Event) -> Task<Message> {
        let Some((session, gateway)) = self.phase.account() else {
            return Task::none();
        };
        if let search::Event::Jump(index) = event {
            if let Some(hit) = self.ui.search.hit(index).cloned() {
                if gateway.timeline.channel_id == Some(hit.channel) {
                    gateway.controls.timeline_intent(history::Intent::JumpTo {
                        channel: hit.channel,
                        message: hit.message,
                    });
                } else {
                    self.ui.search.set_pending_jump(hit.channel, hit.message);
                    match hit.guild {
                        Some(guild) => gateway.controls.select_channel(guild, hit.channel),
                        None => gateway.controls.select_private_channel(hit.channel),
                    }
                }
            }
            return Task::none();
        }
        let Some((guild, channel)) = Self::search_target(gateway) else {
            return Task::none();
        };
        let context = search::Context {
            client: &session.client,
            account: session.user.id,
            guild,
            channel,
        };
        self.ui.search.update(event, &context).map(Message::Search)
    }
}
