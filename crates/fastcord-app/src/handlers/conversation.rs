//! The open conversation: timeline, composer, emoji, attachments and inline media.

use crate::*;

impl App {
    pub(crate) fn conversation_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::AttachmentPurged(succeeded) => {
                self.attachment_purge_failed = !succeeded;
                if !succeeded && matches!(self.phase, Phase::Login) {
                    self.notice =
                        Some("The local attachment cache could not be fully cleared.".to_owned());
                }
            }
            Message::AttachmentLoaded(generation, key, target, result) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    attachment_ui::loaded(gateway, key, target, result);
                }
            }
            Message::AttachmentActionFinished(generation, key, action, outcome) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.attachment_action_workers.remove(&(key, action));
                    attachment_ui::finished(gateway, outcome);
                }
            }
            Message::MediaReady(generation, key, result) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    media_ui::ready(gateway, key, result);
                }
            }
            Message::MediaFrame(id) => {
                if let Some(gateway) = self.phase.gateway()
                    && let Some(session) = gateway
                        .media
                        .as_ref()
                        .and_then(media_ui::MediaSession::video_session)
                {
                    session.consume(id);
                }
            }
            Message::MediaTick => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    media_ui::tick(gateway);
                }
            }
            Message::CloseViewer => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.attachment_viewer = None;
                }
            }
            Message::MarkRead(guild, channel) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.mark_read(guild, channel);
                }
            }
            Message::FirstUnread(channel, acked) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway
                        .controls
                        .timeline_intent(history::Intent::FirstUnread { channel, acked });
                }
            }
            Message::EmojiLoaded(generation, asset, ticket, result) if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == generation) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    return gateway.emoji.loaded(
                        generation,
                        asset,
                        ticket,
                        result,
                        gateway.attachments.as_ref(),
                    );
                }
            }
            Message::AvatarLoaded(generation, key, image) => {
                if let Some(gateway) = self.phase.gateway_mut()
                    && gateway.id == generation
                {
                    return gateway.avatars.loaded(
                        generation,
                        key,
                        image,
                        gateway.attachments.as_ref(),
                    );
                }
            }
            Message::EmojiVisibility(occurrence, visible) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.emoji_visibility(occurrence, visible);
                }
            }
            Message::EmojiPicker(event) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    let reset_scroll = matches!(
                        event,
                        emoji_picker::Event::Toggle
                            | emoji_picker::Event::Search(_)
                            | emoji_picker::Event::Category(_)
                            | emoji_picker::Event::Variants(_)
                    );
                    if matches!(event, emoji_picker::Event::Toggle)
                        && !gateway.timeline.can_send
                        && gateway.composer.editing().is_none()
                    {
                        return Task::none();
                    }
                    if let Some(choice) = gateway.emoji_picker.update(
                        event,
                        gateway.timeline.channel_id,
                        &gateway.emoji_snapshot,
                    ) {
                        let accepted =
                            match (gateway.emoji_picker.reaction(), gateway.timeline.channel_id) {
                                // Opened for a message: the pick reacts to it.
                                (Some(message), Some(channel)) => {
                                    let queued = gateway.controls.add_reaction(
                                        channel,
                                        message,
                                        choice.reaction_emoji(),
                                    );
                                    if !queued {
                                        gateway.composer.refused();
                                    }
                                    queued
                                }
                                // Otherwise it is text for the composer.
                                _ => gateway.composer.insert_emoji(&choice.source()),
                            };
                        if accepted {
                            gateway.emoji_picker.inserted(choice);
                        }
                    }
                    if reset_scroll && gateway.emoji_picker.request.open {
                        return Task::batch([
                            iced::widget::operation::scroll_to(
                                "emoji-list",
                                iced::widget::scrollable::AbsoluteOffset { x: 0.0, y: 0.0 },
                            ),
                            iced::widget::operation::focus("emoji-search"),
                        ]);
                    }
                    if !gateway.emoji_picker.request.open {
                        return iced::widget::operation::focus(composer::EDITOR_ID);
                    }
                }
            }
            Message::Timeline(event) => {
                if let Some((session, gateway)) = self.phase.account_mut() {
                    match event {
                        timeline::Event::CopyText {
                            channel_id,
                            message_id,
                        } => {
                            if let Some(source) =
                                timeline::copy_text(&gateway.timeline, channel_id, message_id)
                            {
                                return iced::clipboard::write(source.to_owned());
                            }
                        }
                        timeline::Event::ReadAloud {
                            channel_id,
                            message_id,
                        } => {
                            if let Some(message) =
                                timeline::message_of(&gateway.timeline, channel_id, message_id)
                            {
                                speech::speak(speech::announcement(message));
                            }
                        }
                        timeline::Event::StopReading => speech::stop(),
                        timeline::Event::Forward {
                            channel_id,
                            message_id,
                        } => {
                            let preview =
                                timeline::copy_text(&gateway.timeline, channel_id, message_id)
                                    .unwrap_or_default()
                                    .to_owned();
                            let source = fastcord_discord::ForwardSource {
                                channel: channel_id,
                                message: message_id,
                                guild: gateway
                                    .navigation
                                    .selected_guild
                                    .filter(|_| !gateway.directs),
                            };
                            return self.forward_event(forward::Event::Open { source, preview });
                        }
                        timeline::Event::Publish {
                            channel_id,
                            message_id,
                        } => {
                            return self.announce_event(announce::Event::AskPublish(
                                channel_id, message_id,
                            ));
                        }
                        timeline::Event::ConfirmPublish {
                            channel_id,
                            message_id,
                        } => {
                            return self.announce_event(announce::Event::ConfirmPublish(
                                channel_id, message_id,
                            ));
                        }
                        timeline::Event::CancelPublish => {
                            return self.announce_event(announce::Event::CancelPublish);
                        }
                        timeline::Event::CopyLink {
                            channel_id,
                            message_id,
                        } => {
                            let guild = gateway
                                .navigation
                                .selected_guild
                                .filter(|_| !gateway.directs);
                            return iced::clipboard::write(message_link::format(
                                guild, channel_id, message_id,
                            ));
                        }
                        timeline::Event::JoinInvite(code) => {
                            return self.invite_event(invite::Event::Join(code));
                        }
                        timeline::Event::OpenGuild(guild) => {
                            return self.handle(Message::SelectGuild(guild));
                        }
                        timeline::Event::CopyId { message_id } => {
                            return iced::clipboard::write(message_id.to_string());
                        }
                        timeline::Event::OpenLink(url) => {
                            if let Some(link) = message_link::parse(&url) {
                                // Only a channel the account can open is selected;
                                // otherwise the wait ends with a notice.
                                jump_to(
                                    &mut self.ui.search,
                                    gateway,
                                    link.guild,
                                    link.channel,
                                    Some(link.message),
                                );
                            } else if let Some(url) = rich::link_target(&url) {
                                open_link(url.as_str());
                            }
                        }
                        timeline::Event::Report {
                            channel_id,
                            message_id,
                        } => {
                            let Some(found) =
                                timeline::report_subject(&gateway.timeline, channel_id, message_id)
                            else {
                                return Task::none();
                            };
                            let guild = gateway
                                .navigation
                                .selected_guild
                                .filter(|_| !gateway.directs);
                            let mut subject = report::Subject::new(
                                fastcord_discord::ReportTarget::Message {
                                    channel: channel_id,
                                    message: message_id,
                                },
                                if found.voice {
                                    "voice message"
                                } else {
                                    "message"
                                },
                                found.author.display_name(),
                            );
                            subject.excerpt = found.excerpt;
                            subject.link =
                                Some(message_link::format(guild, channel_id, message_id));
                            subject.person =
                                Some((found.author.id, found.author.display_name().to_owned()));
                            return self
                                .ui
                                .report
                                .begin(subject, &session.client)
                                .map(Message::Report);
                        }
                        timeline::Event::OpenProfile(user) => {
                            return self
                                .ui
                                .friends
                                .update(friends::Event::Profile(user), &session.client)
                                .map(Message::Friends);
                        }
                        timeline::Event::StartThread {
                            channel_id,
                            message_id,
                        } => {
                            if let Some(selection) = gateway
                                .navigation
                                .selection
                                .as_ref()
                                .filter(|s| s.channel_id == channel_id)
                            {
                                let first =
                                    timeline::copy_text(&gateway.timeline, channel_id, message_id)
                                        .unwrap_or_default()
                                        .to_owned();
                                return self
                                    .ui
                                    .threads
                                    .update(
                                        threads::Event::Start(
                                            selection.guild_id,
                                            channel_id,
                                            message_id,
                                            first,
                                        ),
                                        &session.client,
                                    )
                                    .map(Message::Threads);
                            }
                        }
                        // A thread this client has never held cannot be opened
                        // from here; the archive list brings it in.
                        timeline::Event::OpenThread(thread) => {
                            if let Some(guild) = gateway.navigation.selected_guild {
                                self.ui.threads.close();
                                gateway.controls.select_channel(guild, thread);
                            }
                        }
                        timeline::Event::ToggleSpoiler {
                            message_id,
                            spoiler,
                        } => gateway.toggle_spoiler(message_id, spoiler),
                        timeline::Event::EmojiVisibility(occurrence, visible) => {
                            gateway.emoji_visibility(occurrence, visible);
                        }
                        timeline::Event::LoadAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::load_preview(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                            );
                        }
                        timeline::Event::PlayAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return media_ui::play(
                                gateway,
                                session,
                                self.audio_preferences.output.choice.clone(),
                                channel_id,
                                message_id,
                                attachment_id,
                            );
                        }
                        timeline::Event::Media(control) => media_ui::control(gateway, control),
                        timeline::Event::ViewAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::open_viewer(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                            );
                        }
                        timeline::Event::DownloadAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::start_action(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                                AttachmentAction::Download,
                            );
                        }
                        timeline::Event::OpenAttachment {
                            channel_id,
                            message_id,
                            attachment_id,
                        } => {
                            return attachment_ui::start_action(
                                gateway,
                                session,
                                channel_id,
                                message_id,
                                attachment_id,
                                AttachmentAction::Open,
                            );
                        }
                        timeline::Event::Viewport {
                            channel_id,
                            viewport,
                        } => gateway.controls.timeline_viewport(channel_id, viewport),
                        timeline::Event::Measured {
                            channel_id,
                            measurement,
                        } => gateway.controls.timeline_measured(channel_id, measurement),
                        timeline::Event::JumpLatest { channel_id } => {
                            gateway
                                .controls
                                .timeline_intent(history::Intent::Latest(channel_id));
                        }
                        timeline::Event::JumpTo {
                            channel_id,
                            message_id,
                        } => gateway.controls.timeline_intent(history::Intent::JumpTo {
                            channel: channel_id,
                            message: message_id,
                        }),
                        timeline::Event::SetPinned {
                            channel_id,
                            message_id,
                            pinned,
                        } => {
                            return self.pins_event(pins::Event::SetPinned(
                                channel_id, message_id, pinned,
                            ));
                        }
                        timeline::Event::LoadOlder { channel_id } => {
                            gateway
                                .controls
                                .timeline_intent(history::Intent::Older(channel_id));
                        }
                        timeline::Event::Retry { channel_id } => {
                            gateway
                                .controls
                                .timeline_intent(history::Intent::Retry(channel_id));
                        }
                        action @ (timeline::Event::Edit { .. }
                        | timeline::Event::Reply { .. }
                        | timeline::Event::Delete { .. }
                        | timeline::Event::ConfirmDelete { .. }
                        | timeline::Event::CancelDelete
                        | timeline::Event::SelectMode { .. }
                        | timeline::Event::ToggleSelect { .. }
                        | timeline::Event::AskBulkDelete { .. }
                        | timeline::Event::CancelBulkDelete
                        | timeline::Event::ConfirmBulkDelete { .. }
                        | timeline::Event::AskReactionRemoval(_)
                        | timeline::Event::CancelReactionRemoval
                        | timeline::Event::ConfirmReactionRemoval
                        | timeline::Event::RetryChange { .. }
                        | timeline::Event::DismissChange { .. }
                        | timeline::Event::ToggleReaction { .. }
                        | timeline::Event::InspectReaction { .. }
                        | timeline::Event::CloseReactionUsers
                        | timeline::Event::PollVote { .. }
                        | timeline::Event::PollEnd { .. }
                        | timeline::Event::SetEmbeds { .. }
                        | timeline::Event::More(_)
                        | timeline::Event::PollVoters { .. }
                        | timeline::Event::Component { .. }
                        | timeline::Event::DismissEphemeral { .. }
                        | timeline::Event::React { .. }) => {
                            if gateway.message_action(action) {
                                return iced::widget::operation::focus(composer::EDITOR_ID);
                            }
                        }
                    }
                }
            }
            Message::FilesPicked(paths) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.composer.attach(paths);
                }
            }
            Message::ImagePasted(result) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    match result {
                        Ok(path) => gateway.composer.attach_pasted(&path),
                        Err(reason) => gateway.composer.paste_failed(reason),
                    }
                }
            }
            Message::Composer(composer::Event::Plus) => {
                return self.handle(Message::OpenMenu(Some(Menu::Composer)));
            }
            Message::Expressions(tab) => {
                let Some(gateway) = self.phase.gateway() else {
                    return Task::none();
                };
                let open = self.expressions_tab(gateway);
                let mut steps = Vec::new();
                for (each, message) in [
                    (
                        Expressions::Emoji,
                        Message::EmojiPicker(emoji_picker::Event::Toggle),
                    ),
                    (Expressions::Gif, Message::Gif(gif::Event::Toggle)),
                    (
                        Expressions::Sticker,
                        Message::Sticker(sticker::Event::Toggle),
                    ),
                ] {
                    // Toggle off the open tab, toggle on the chosen one.
                    if (open == Some(each)) != (tab == each) {
                        steps.push(message);
                    }
                }
                let tasks: Vec<_> = steps.into_iter().map(|m| self.handle(m)).collect();
                return Task::batch(tasks);
            }
            Message::Composer(composer::Event::Toggle(composer::Picker::Emoji))
                if matches!(&self.phase, Phase::Account { gateway, .. }
                    if self.expressions_tab(gateway).is_some_and(|tab| tab != Expressions::Emoji)) =>
            {
                // The smiley closes the window whichever tab is shown.
                let Some(gateway) = self.phase.gateway() else {
                    return Task::none();
                };
                let tab = self.expressions_tab(gateway);
                return self.handle(match tab {
                    Some(Expressions::Gif) => Message::Gif(gif::Event::Toggle),
                    _ => Message::Sticker(sticker::Event::Toggle),
                });
            }
            Message::Composer(composer::Event::Toggle(picker)) => {
                return self.handle(match picker {
                    composer::Picker::Emoji => Message::EmojiPicker(emoji_picker::Event::Toggle),
                    composer::Picker::Poll => Message::Poll(poll_create::Event::Toggle),
                });
            }
            Message::Composer(composer::Event::PasteImage) => {
                if matches!(self.phase, Phase::Account { .. }) {
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(paste::paste_image)
                                .await
                                .unwrap_or(Err("The pasted image could not be read."))
                        },
                        Message::ImagePasted,
                    );
                }
            }
            Message::FileDropped(path) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.composer.attach(vec![path]);
                }
            }
            Message::Composer(composer::Event::Attach) => {
                if matches!(self.phase, Phase::Account { .. }) {
                    return Task::perform(
                        async {
                            rfd::AsyncFileDialog::new()
                                .set_title("Attach files")
                                .pick_files()
                                .await
                                .map(|files| {
                                    files
                                        .into_iter()
                                        .map(|file| file.path().to_owned())
                                        .collect()
                                })
                                .unwrap_or_default()
                        },
                        Message::FilesPicked,
                    );
                }
            }
            Message::Composer(event) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    // A clicked completion leaves the focus on its button; take it back.
                    let refocus = matches!(
                        event,
                        composer::Event::Send
                            | composer::Event::Edit(_)
                            | composer::Event::Complete { .. }
                    );
                    // Typing is announced only by a focused window's edits.
                    let typing = self
                        .window_active
                        .then(|| gateway.composer.typing_channel(&event))
                        .flatten()
                        .filter(|channel| {
                            gateway.timeline.can_send
                                && gateway.timeline.channel_id == Some(*channel)
                        });
                    composer::update(
                        &mut gateway.composer,
                        event,
                        &gateway.timeline,
                        &gateway.controls,
                    );
                    if let Some(channel) = typing {
                        gateway.controls.typing(channel);
                    }
                    let slash = self.slash_typed();
                    if refocus {
                        return Task::batch([
                            iced::widget::operation::focus(composer::EDITOR_ID),
                            slash,
                        ]);
                    }
                    return slash;
                }
            }
            _ => {}
        }
        Task::none()
    }
}
