//! Voice and video calls: joining, streams, screen share, camera and volumes.

use crate::*;

impl App {
    pub(crate) fn call_message(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::ToggleVoiceDetails => {
                self.ui.voice_details = !self.ui.voice_details;
                if !self.ui.voice_details
                    && let Some(writer) = &self.volume_writer
                {
                    writer.flush();
                }
            }
            Message::JoinVoice(guild, channel) => {
                // Joins the call only; the open chat stays as it is.
                if let Some(gateway) = self.phase.gateway_mut() {
                    let join = gateway.navigation.selected_guild == Some(guild)
                        && gateway.navigation.channels.rows.iter().any(|row| {
                            row.id == channel
                                && row.kind
                                    == fastcord_discord::state::navigation::ChannelKind::Voice
                                && row.enabled
                                && row.permissions.connect
                        });
                    let vad_forbidden = gateway
                        .navigation
                        .channels
                        .rows
                        .iter()
                        .any(|row| row.id == channel && !row.permissions.use_vad);
                    if join {
                        self.ui.stage.reset();
                        gateway.controls.set_vad_forbidden(vad_forbidden);
                        gateway.voice_guild = Some(guild);
                        gateway.voice_call = None;
                        let controls = gateway.controls.voice_controls();
                        gateway
                            .controls
                            .request_voice_state(VoiceStateRequest::join(
                                Some(guild),
                                channel,
                                controls.self_muted,
                                controls.deafened,
                            ));
                    }
                }
            }
            Message::StartCall(channel) | Message::AcceptCall(channel) => {
                if let Some((session, gateway)) = self.phase.account_mut() {
                    let busy = gateway.voice_call == Some(channel)
                        && gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Idle;
                    if !busy {
                        // One parent voice connection: this request replaces a
                        // guild voice join (the correlator moves away from it).
                        gateway.voice_guild = None;
                        gateway.controls.set_vad_forbidden(false);
                        gateway.voice_call = Some(channel);
                        let controls = gateway.controls.voice_controls();
                        gateway
                            .controls
                            .request_voice_state(VoiceStateRequest::join(
                                None,
                                channel,
                                controls.self_muted,
                                controls.deafened,
                            ));
                        if matches!(message, Message::StartCall(_)) {
                            return call_signal(session, channel, CallSignal::Ring);
                        }
                    }
                }
            }
            Message::DeclineCall(channel) => {
                if let Some(session) = self.phase.session() {
                    return call_signal(session, channel, CallSignal::StopRinging);
                }
            }
            Message::CallSignalled(result) => {
                self.notice = result.err().map(|error| match error {
                    RestError::WriteNotVerified(_) => error.to_string(),
                    _ => "The call request failed; try again.".to_owned(),
                });
            }
            Message::LeaveVoice => {
                if let Some(writer) = &self.volume_writer {
                    writer.flush();
                }
                self.ui.stage.reset();
                let mut cancel = None;
                if let Some((session, gateway)) = self.phase.account_mut() {
                    let call = gateway.voice_call.take();
                    if let Some(guild) = gateway.voice_guild.take().map(Some).or(call.map(|_| None))
                    {
                        gateway
                            .controls
                            .request_voice_state(VoiceStateRequest::leave(guild));
                    }
                    // Hanging up before anyone answered also stops the ringing.
                    cancel = call
                        .filter(|channel| {
                            gateway.navigation.calls.iter().any(|call| {
                                call.channel_id == *channel
                                    && !call.ringing.is_empty()
                                    && call
                                        .participants
                                        .iter()
                                        .all(|user| *user == session.user.id)
                            })
                        })
                        .map(|channel| call_signal(session, channel, CallSignal::StopRinging));
                }
                if let Some(gateway) = self.phase.gateway_mut() {
                    for session in gateway.controls.watched() {
                        if !gateway.controls.unwatch_stream(session.key.clone()) {
                            self.notice = Some(
                                "Could not queue Stop watching; retry before leaving voice.".into(),
                            );
                        }
                    }
                    gateway.call_view = call_layout::CallView::default();
                }
                if let Some(task) = cancel {
                    return task;
                }
            }
            Message::ActivateParticipant(user) => {
                self.ui.member_menu.close();
                if let Some((session, gateway)) = self.phase.account_mut() {
                    if user == session.user.id {
                        return Task::none();
                    }
                    let voice = gateway.controls.voice_controls();
                    if let Some(offer) = voice.streams.iter().find(|offer| offer.owner == user) {
                        return self.handle(Message::WatchStream(offer.key.clone()));
                    }
                    if voice
                        .participants
                        .iter()
                        .any(|participant| participant.user_id == user)
                    {
                        gateway.call_view.open = true;
                    } else {
                        self.notice =
                            Some("Join this person's voice channel to watch their stream.".into());
                    }
                }
            }
            Message::WatchStream(key) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    let controls = gateway.controls.voice_controls();
                    if gateway.controls.voice_audio_state() != gateway::VoiceAudioState::Running
                        || !controls.streams.iter().any(|offer| offer.key == key)
                    {
                        self.notice = Some(
                            "Could not watch this stream. Join its voice channel and retry.".into(),
                        );
                    } else {
                        let previous = gateway
                            .controls
                            .watched_stream(&key)
                            .filter(|session| session.failed());
                        if previous.is_some() && !gateway.controls.unwatch_stream(key.clone()) {
                            self.notice = Some("Could not queue Retry watching; retry.".into());
                            return Task::none();
                        }
                        match gateway.controls.watch_stream(key.clone()) {
                            Ok(()) => {
                                if let Some(previous) = previous
                                    && let Some(session) = gateway.controls.watched_stream(&key)
                                {
                                    session.set_audio(previous.audio_settings());
                                }
                                gateway.call_view.open = true;
                                self.notice = None;
                            }
                            Err(reason) => self.notice = Some(reason.into()),
                        }
                    }
                }
            }
            Message::SetStreamVolume(key, percent) => {
                if let Some(gateway) = self.phase.gateway()
                    && let Some(session) = gateway.controls.watched_stream(&key)
                {
                    session.set_audio(playback::AudioSettings {
                        percent,
                        ..session.audio_settings()
                    });
                }
            }
            Message::MuteStream(key, muted) => {
                if let Some(gateway) = self.phase.gateway()
                    && let Some(session) = gateway.controls.watched_stream(&key)
                {
                    session.set_audio(playback::AudioSettings {
                        muted,
                        ..session.audio_settings()
                    });
                }
            }
            Message::UnwatchStream(key) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    if gateway.controls.unwatch_stream(key) {
                        self.notice = None;
                    } else {
                        self.notice = Some("Could not queue Stop watching; retry.".into());
                    }
                }
            }
            Message::OpenCall => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    if let Some((Some(guild), channel)) = gateway.controls.voice_target() {
                        self.ui.threads.close();
                        gateway.controls.select_channel(guild, channel);
                    }
                    gateway.call_view.open = gateway.voice_guild.or(gateway.voice_call).is_some();
                }
            }
            Message::ShowStream(open) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    let any = !gateway.controls.watched().is_empty()
                        || !gateway.controls.camera_tiles().snapshot().remote.is_empty()
                        || gateway.controls.voice_controls().camera.wanted;
                    gateway.call_view.open = open && any;
                    if !open {
                        gateway.call_view.full = None;
                    }
                }
            }
            Message::SetCallMode(mode) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.call_view.mode = mode;
                    gateway.call_view.full = None;
                }
            }
            Message::PinTile(id) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.call_view.toggle_pin(id);
                    gateway.call_view.full = None;
                }
            }
            Message::FullTile(id) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.call_view.full = id;
                    gateway.call_view.open |= gateway.call_view.full.is_some();
                }
            }
            Message::SetHidePeople(hide) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.call_view.hide_people = hide;
                }
            }
            Message::OpenSharePicker => {
                if let Some(gateway) = self.phase.gateway_mut()
                    && !gateway.controls.share_view().active
                    && !gateway.share_picker.is_open()
                {
                    gateway.share_picker = gateway::SharePicker::Loading;
                    // Enumeration touches the window system: never on the UI thread.
                    return Task::perform(
                        async {
                            tokio::task::spawn_blocking(|| {
                                fastcord_platform::capture::availability()?;
                                let sources = fastcord_platform::capture::list_sources()?;
                                // Only where the user chooses; a missing daemon
                                // just leaves the list empty.
                                let audio = if gateway::AudioOffer::current()
                                    == gateway::AudioOffer::Choose
                                {
                                    fastcord_platform::loopback::list_targets().unwrap_or_default()
                                } else {
                                    Vec::new()
                                };
                                Ok::<_, fastcord_platform::capture::CaptureError>((sources, audio))
                            })
                            .await
                            .map_err(|_| "Listing screens and windows failed.".to_owned())
                            .and_then(|listed| listed.map_err(|error| error.to_string()))
                        },
                        Message::ShareSources,
                    );
                }
            }
            Message::ShareSources(result) => {
                if let Some(gateway) = self.phase.gateway_mut()
                    && matches!(gateway.share_picker, gateway::SharePicker::Loading)
                {
                    gateway.share_picker = match result {
                        Ok((sources, audio)) => {
                            if !audio
                                .iter()
                                .any(|source| Some(source.target) == gateway.share_audio_target)
                            {
                                gateway.share_audio_target = None;
                            }
                            gateway.share_audio_sources = audio;
                            gateway::SharePicker::Listed(sources)
                        }
                        Err(error) => gateway::SharePicker::Failed(error),
                    };
                }
            }
            Message::ToggleShareAudio(on) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.share_audio = on;
                }
            }
            Message::ChooseShareAudio(target) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.share_audio_target = Some(target);
                }
            }
            Message::CloseSharePicker => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    gateway.share_picker = gateway::SharePicker::Closed;
                }
            }
            Message::StartShare(id) => {
                if let Some(gateway) = self.phase.gateway_mut() {
                    let source = match &gateway.share_picker {
                        gateway::SharePicker::Listed(sources) => {
                            sources.iter().find(|source| source.id == id).cloned()
                        }
                        _ => None,
                    };
                    gateway.share_picker = gateway::SharePicker::Closed;
                    let target = gateway.controls.voice_target().filter(|_| {
                        gateway.controls.voice_audio_state() == gateway::VoiceAudioState::Running
                    });
                    if let (Some(source), Some((guild, channel))) = (source, target) {
                        let request = match guild {
                            Some(guild) => {
                                fastcord_model::StreamCreateRequest::guild(guild, channel)
                            }
                            None => fastcord_model::StreamCreateRequest::call(channel),
                        };
                        let offer = gateway::AudioOffer::current();
                        let audio = gateway
                            .share_audio
                            .then(|| offer.scope(source.id, gateway.share_audio_target))
                            .flatten();
                        self.notice = (gateway.share_audio
                            && offer == gateway::AudioOffer::Choose
                            && audio.is_none())
                        .then(|| {
                            "No audio source was chosen, so only the picture is shared.".into()
                        });
                        gateway.controls.start_share(source, request, audio);
                    } else {
                        self.notice =
                            Some("Join a voice channel before sharing your screen.".into());
                    }
                }
            }
            Message::StopShare => {
                if let Some(gateway) = self.phase.gateway()
                    && !gateway.controls.stop_share()
                {
                    self.notice = Some("Could not stop sharing; try again.".into());
                }
            }
            Message::VideoReady(id) => {
                if let Some(gateway) = self.phase.gateway() {
                    for session in gateway.controls.watched() {
                        session.consume(id);
                    }
                    let tiles = gateway.controls.camera_tiles().snapshot();
                    for session in tiles
                        .preview
                        .iter()
                        .chain(tiles.remote.iter().map(|(_, session)| session))
                    {
                        session.consume(id);
                    }
                }
            }
            Message::ToggleCamera => {
                if let Some(gateway) = self.phase.gateway() {
                    // The only way the camera turns on: an explicit click, in a
                    // running call.
                    let on = !gateway.controls.voice_controls().camera.wanted;
                    if !on
                        || gateway.controls.voice_audio_state() == gateway::VoiceAudioState::Running
                    {
                        if on {
                            // The saved device, effect and mirror apply from
                            // the first picture.
                            let saved = &self.audio_preferences.camera;
                            gateway.controls.prepare_camera(
                                saved.id.clone(),
                                saved.effect,
                                saved.mirror,
                            );
                        }
                        gateway.controls.set_camera(on);
                    }
                }
            }
            Message::ListCameras => {
                if !self.camera_list.probing {
                    self.camera_list.probing = true;
                    return Task::perform(gateway::list_cameras(), Message::CamerasListed);
                }
            }
            Message::CamerasListed(result) => {
                self.camera_list.probing = false;
                self.camera_list.probed = true;
                match result {
                    Ok(devices) => {
                        self.camera_list.devices = devices;
                        self.camera_list.error = None;
                    }
                    Err(error) => self.camera_list.error = Some(error),
                }
            }
            Message::SelectCamera(choice) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.select_camera(choice.id.clone());
                }
                self.audio_preferences.camera = audio_devices::CameraSelection {
                    id: choice.id,
                    label: choice.label,
                    ..self.audio_preferences.camera.clone()
                };
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SelectCameraEffect(effect) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.set_camera_effect(effect);
                }
                self.audio_preferences.camera.effect = effect;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SetMirrorPreview(mirror) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.set_camera_mirror(mirror);
                }
                self.audio_preferences.camera.mirror = mirror;
                if self.audio_saving.is_none() {
                    return self.save_audio_preferences();
                }
            }
            Message::SetSelfMute(muted) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.set_self_muted(muted);
                }
            }
            Message::SetDeafen(deafened) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.set_deafened(deafened);
                }
            }
            // Both keyboard/drag changes and presets reach the same bounded writer.
            Message::SetParticipantVolume(user, percent)
            | Message::PresetParticipantVolume(user, percent) => {
                if let Some(gateway) = self.phase.gateway() {
                    gateway.controls.set_volume_percent(user, percent);
                    if let Some(writer) = &self.volume_writer
                        && !writer.set(user, percent)
                    {
                        self.volume_notice =
                            voice_settings::Status::Failed.notice().map(str::to_owned);
                    }
                }
            }
            Message::CommitParticipantVolume(user) => {
                if let Some(writer) = &self.volume_writer
                    && let Some(gateway) = self.phase.gateway()
                {
                    writer.set(user, gateway.controls.voice_controls().volume_percent(user));
                    writer.flush();
                }
            }
            Message::VolumeSettingsStatus(id, status) => {
                if matches!(&self.phase, Phase::Account { gateway, .. } if gateway.id == id) {
                    self.volume_notice = status.notice().map(str::to_owned);
                }
            }
            _ => {}
        }
        Task::none()
    }
}
