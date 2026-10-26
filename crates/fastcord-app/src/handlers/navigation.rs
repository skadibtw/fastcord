//! Servers, channels and direct messages: the worker's snapshots, selection, menus, friends.

use crate::*;

impl App {
    pub(crate) fn navigation_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::MenuPick(message) => {
                self.close_menus();
                return self.handle(*message);
            }
            Message::OpenMenu(menu) => {
                self.close_menus();
                self.ui.menu = menu;
            }
            Message::NewDm(new_dm::Event::Create) => {
                let recipients = self.ui.new_dm.recipients();
                let _ = self.handle(Message::PrivateRecipients(recipients));
                if let Some((session, gateway)) = self.phase.account()
                    && parse_private_recipients(&gateway.private.recipients, session.user.id)
                        .is_ok()
                {
                    self.ui.new_dm.update(new_dm::Event::Close);
                }
                return self.handle(Message::OpenPrivate);
            }
            Message::NewDm(event) => {
                let opening = matches!(event, new_dm::Event::Open);
                self.ui.new_dm.update(event);
                if opening {
                    self.close_menus();
                    if let Some(gateway) = self.phase.gateway_mut() {
                        gateway.private.notice = None;
                    }
                    return iced::widget::operation::focus("new-dm-search");
                }
            }
            Message::ShowRequests => {
                self.ui.friends.close();
                self.ui.requests_open = true;
                let mut tasks = vec![self.handle(Message::ShowDirects(true))];
                if !self.ui.safety.is_open() {
                    tasks.push(self.handle(Message::Safety(safety::Event::Toggle)));
                }
                tasks.push(self.handle(Message::Safety(safety::Event::Tab(safety::Tab::Requests))));
                return Task::batch(tasks);
            }
            Message::Gateway(id, status) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == id) =>
            {
                if status == GatewayStatus::AuthenticationRequired
                    || matches!(
                        &status,
                        GatewayStatus::Stopped(
                            fastcord_discord::gateway::StopReason::ActionRequired(_)
                        )
                    )
                {
                    // Cancel panels and their in-flight tasks as on logout,
                    // including requests outside the Gateway subscription.
                    let cache = self.teardown_account().and_then(|(_, cache)| cache);
                    self.reset_login();
                    self.notice = Some(
                        "Discord no longer accepts this login, or requires an account check. All account connections and requests have stopped. Open the official Discord client to check Account Standing and complete any verification or password reset before signing in here again. A saved copy of the old token can be removed with Forget saved login.".to_owned(),
                    );
                    return cache.map_or(Task::none(), |cache| {
                        Task::perform(async move { cache.purge().await }, |result| {
                            Message::AttachmentPurged(result.is_ok())
                        })
                    });
                } else if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.status = status;
                }
            }
            Message::Navigation(id) => {
                let before = match &self.phase {
                    Phase::Account { gateway, .. } => gateway.timeline.channel_id,
                    _ => None,
                };
                let status = if let Some(gateway) = self.phase.gateway_mut()
                    && gateway.id == id
                {
                    gateway.consume()
                } else {
                    None
                };
                // Every snapshot carries the last status, so applying it must not
                // end the update: the follow-up below runs unless it ended the account.
                let mut status_task = Task::none();
                if let Some(status) = status {
                    status_task = self.handle(Message::Gateway(id, status));
                    if !matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == id) {
                        return status_task;
                    }
                }
                if let Some(gateway) = self.phase.gateway()
                    && let Some(message) = self
                        .ui
                        .search
                        .take_pending_jump(gateway.timeline.channel_id)
                    && let Some(channel) = gateway.timeline.channel_id
                {
                    gateway
                        .controls
                        .timeline_intent(history::Intent::JumpTo { channel, message });
                }
                if self.ui.search.take_jump_expired() || self.ui.inbox.take_expired() {
                    self.notice = Some("That message is in a channel you cannot open.".to_owned());
                }
                if let Some(gateway) = self.phase.gateway()
                    && let Some((channel, target)) =
                        self.ui.inbox.take_pending(gateway.timeline.channel_id)
                {
                    inbox_jump(&gateway.controls, channel, target);
                }
                let mut invite_task = Task::none();
                if let Some((session, gateway)) = self.phase.account() {
                    self.ui.sticker.sync(gateway.timeline.channel_id);
                    self.ui.invite.cache.sync_guilds(&gateway.navigation);
                    invite_task = self
                        .ui
                        .invite
                        .pump(&gateway.timeline.rows, &session.client)
                        .map(Message::Invite);
                    self.ui.forward.sync(gateway.timeline.channel_id);
                    self.ui.announce.sync(gateway.timeline.channel_id);
                    self.ui.group.sync(
                        gateway
                            .navigation
                            .private_selection
                            .as_ref()
                            .map(|selection| selection.channel_id),
                    );
                    self.ui.identity.sync(&gateway.navigation);
                    self.ui.safety.sync(&gateway.navigation);
                    self.ui
                        .report
                        .sync(self.ui.report.watched().and_then(|(channel, message)| {
                            timeline::is_retained(&gateway.timeline, channel, message)
                        }));
                    self.ui.poll.sync(gateway.timeline.channel_id);
                    self.ui.slash.sync(gateway.timeline.channel_id);
                    self.ui
                        .slash
                        .modal_shown(gateway.timeline.interaction.modal.as_ref());
                    self.ui.voice.sync(gateway.timeline.channel_id);
                }
                // A navigation the user started (keys, switcher, sidebar) has
                // landed: the composer takes focus back, and the sidebar shows
                // servers or direct messages to match what opened.
                let mut refocus = Task::none();
                if self.ui.nav_pending
                    && let Some(gateway) = self.phase.gateway_mut()
                    && gateway.timeline.channel_id.is_some()
                    && gateway.timeline.channel_id != before
                {
                    self.ui.nav_pending = false;
                    gateway.directs = gateway.navigation.private_selection.is_some();
                    refocus = iced::widget::operation::focus(composer::EDITOR_ID);
                }
                self.observe_ui_sound();
                return Task::batch([
                    status_task,
                    refocus,
                    self.soundboard_follow(),
                    self.assets_follow(),
                    invite_task,
                ]);
            }
            Message::ShowDirects(on) => {
                self.close_expressions();
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.directs = on;
                }
            }
            Message::VoiceRecordingToggled(on) => {
                self.audio_preferences.voice_recording = on;
                if !on {
                    self.ui.voice.reset();
                }
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::Status(event) => {
                if let Some((session, gateway)) = self.phase.account() {
                    let own = gateway.navigation.presences.get(&session.user.id);
                    return self
                        .ui
                        .status
                        .update(event, own, &session.client)
                        .map(Message::Status);
                }
            }
            Message::Friends(event) => {
                self.ui.requests_open = false;
                if matches!(event, friends::Event::Open) {
                    self.close_expressions();
                }
                if matches!(
                    event,
                    friends::Event::Profile(_)
                        | friends::Event::OpenDm(_)
                        | friends::Event::Report(_)
                ) {
                    self.ui.member_menu.close();
                }
                if let Some((session, gateway)) = self.phase.account_mut() {
                    match event {
                        friends::Event::OpenDm(user) => {
                            self.ui.friends.close();
                            gateway.directs = true;
                            gateway.private.notice = Some(
                                if gateway.controls.open_private_channel(vec![user]) {
                                    "Opening the conversation…"
                                } else {
                                    "The conversation request could not be queued."
                                }
                                .to_owned(),
                            );
                        }
                        friends::Event::Report(user) => {
                            let mut subject = report::Subject::new(
                                fastcord_discord::ReportTarget::User(user.id),
                                "user",
                                user.display_name(),
                            );
                            subject.person = Some((user.id, user.display_name().to_owned()));
                            let _ = self
                                .ui
                                .friends
                                .update(friends::Event::CloseProfile, &session.client);
                            return self
                                .ui
                                .report
                                .begin(subject, &session.client)
                                .map(Message::Report);
                        }
                        event => {
                            if matches!(event, friends::Event::Open) {
                                gateway.directs = true;
                            }
                            return self
                                .ui
                                .friends
                                .update_for(
                                    event,
                                    &gateway.navigation.relationships,
                                    &session.client,
                                )
                                .map(Message::Friends);
                        }
                    }
                }
            }
            Message::SelectGuild(guild) => {
                self.close_expressions();
                self.ui.requests_open = false;
                self.ui.member_menu.close();
                self.ui.threads.close();
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.directs = false;
                    gateway.controls.select_guild(guild);
                    return iced::widget::operation::scroll_to(
                        "channel-list",
                        iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                    );
                }
            }
            Message::SelectChannel(guild, channel) => {
                self.close_expressions();
                self.ui.member_menu.close();
                // Opens a channel's messages (a voice or Stage channel's chat
                // included); it never touches the voice connection.
                self.ui.threads.close();
                self.ui.nav_pending = true;
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.controls.select_channel(guild, channel);
                }
            }
            Message::SelectPrivateChannel(channel) => {
                self.close_expressions();
                self.ui.requests_open = false;
                self.ui.threads.close();
                self.ui.nav_pending = true;
                if let Some(gateway) = self.phase.gateway_mut() {
                    self.ui.friends.close();
                    gateway.directs = true;
                    gateway.controls.select_private_channel(channel);
                }
            }
            Message::PrivateRecipients(value) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    if value.len() <= MAX_PRIVATE_RECIPIENT_INPUT_BYTES {
                        gateway.private.recipients = value;
                        gateway.private.notice = None;
                    } else {
                        gateway.private.notice =
                            Some("Recipient selection is limited to 256 UTF-8 bytes.".to_owned());
                    }
                }
            }
            Message::OpenPrivate => {
                if let Some((session, gateway)) = self.phase.account_mut() {
                    match parse_private_recipients(&gateway.private.recipients, session.user.id) {
                        Ok(recipients) => {
                            gateway.private.notice =
                                Some(if gateway.controls.open_private_channel(recipients) {
                                    "Opening the selected private conversation…".to_owned()
                                } else {
                                    "The private conversation request could not be queued."
                                        .to_owned()
                                });
                        }
                        Err(error) => gateway.private.notice = Some(error.to_owned()),
                    }
                }
            }
            Message::GuildViewport(window) | Message::ChannelViewport(window) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway
                        .controls
                        .viewport(matches!(message, Message::GuildViewport(_)), window);
                }
            }
            Message::PrivateViewport(window) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.private_viewport(window);
                }
            }
            _ => {}
        }
        Task::none()
    }
}
