//! The application's own tests: login, account lifecycle, routing and the
//! subscriptions `App` drives.

use super::*;

#[test]
fn participant_volume_change_survives_account_teardown_without_release() {
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::MemoryOnly);
    let path = std::env::temp_dir()
        .join(format!(
            "fastcord-app-volume-{}-{}",
            std::process::id(),
            session.user.id
        ))
        .join("voice-settings.csv");
    let (writer, _) = voice_settings::Writer::at(Some(path.clone()), session.user.id);
    app.volume_writer = Some(writer);
    let _ = app.handle(Message::SetParticipantVolume(Snowflake(99), 67));
    let _ = app.handle(Message::PresetParticipantVolume(Snowflake(98), 6));
    let _ = app.handle(Message::SetParticipantVolume(Snowflake(99), 62));
    if let Some(gateway) = app.phase.gateway() {
        assert_eq!(
            gateway
                .controls
                .voice_controls()
                .volume_percent(Snowflake(99)),
            62
        );
    }
    app.teardown_account();
    let restored = gateway::NavigationBridge::new();
    restored.set_volume_settings(voice_settings::load_from(&path, session.user.id));
    assert_eq!(restored.voice_controls().volume_percent(Snowflake(99)), 62);
    assert_eq!(restored.voice_controls().volume_percent(Snowflake(98)), 6);
    assert!(voice_settings::load_from(&path, Snowflake(session.user.id.0 + 1)).is_empty());
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

fn login_app() -> App {
    let (mut app, _) = App::boot();
    app.phase = Phase::Login;
    app
}

fn authenticate(app: &mut App, persistence: Persistence) -> Arc<Session> {
    let session = login::fixture_session();
    app.phase = Phase::Authenticating {
        saved_account: None,
    };
    let _ = app.update(Message::LoginFinished(Ok(LoginOutcome::Authenticated(
        Arc::clone(&session),
        persistence,
    ))));
    session
}

#[test]
fn updates_that_leave_the_timeline_alone_do_not_rescan_it() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let _ = app.synchronize_emoji();
    let scans = app.timeline_wants.scans;
    assert_eq!(scans, 1);
    for _ in 0..3 {
        let _ = app.update(Message::Composer(composer::Event::Attach));
        let _ = app.synchronize_emoji();
    }
    assert_eq!(app.timeline_wants.scans, scans);
    // A new snapshot is scanned again.
    if let Some(gateway) = app.phase.gateway_mut() {
        gateway.timeline = Arc::new(timeline::Snapshot::default());
    }
    let _ = app.synchronize_emoji();
    assert_eq!(app.timeline_wants.scans, scans + 1);
}

#[test]
fn autostart_shows_the_result_when_it_arrives_and_ignores_toggles_meanwhile() {
    let mut app = login_app();
    assert_eq!(app.autostart, None);
    let _ = app.update(Message::AutostartDone(Ok(false)));
    assert_eq!(app.autostart, Some(false));
    let _ = app.update(Message::AutostartToggled(true));
    assert!(app.autostart_busy);
    assert_eq!(
        app.autostart,
        Some(false),
        "the old value stays until the write ends"
    );
    let _ = app.update(Message::AutostartToggled(false));
    let _ = app.update(Message::AutostartDone(Err("denied".to_owned())));
    assert!(!app.autostart_busy);
    assert_eq!(app.autostart, Some(false));
    assert_eq!(app.notice.as_deref(), Some("denied"));
    let _ = app.update(Message::AutostartDone(Ok(true)));
    assert_eq!(app.autostart, Some(true));
}

#[test]
fn an_inactive_window_drops_decoded_animations() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    let frame = attachments::DecodedImage {
        width: 1,
        height: 1,
        rgba: bytes::Bytes::from_static(&[255, 0, 0, 255]),
    };
    gateway.emoji.views.insert(
        emoji::Asset::Custom {
            id: Snowflake(12345),
            animated: true,
        },
        emoji::View::from_decoded(attachments::DecodedAnimation {
            frames: vec![frame.clone(), frame],
            delays: vec![std::time::Duration::from_millis(20); 2],
        }),
    );
    let _ = app.update(Message::WindowActive(false));
    let Some(gateway) = app.phase.gateway() else {
        panic!("account");
    };
    assert!(gateway.emoji.views.is_empty());
}

#[test]
fn tcp_proxy_explains_media_before_any_join_or_ring_intent() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    app.network = app.network.with_mode(fastcord_net::settings::Mode::Proxy);
    for message in [
        Message::JoinVoice(Snowflake(1), Snowflake(2)),
        Message::StartCall(Snowflake(3)),
        Message::AcceptCall(Snowflake(3)),
    ] {
        drop(app.update(message));
        assert!(app.network_settings.open);
        assert!(app.network_settings.notice.unwrap().contains("TCP only"));
        let Some(gateway) = app.phase.gateway() else {
            panic!("account expected")
        };
        assert_eq!(
            gateway.controls.voice_audio_state(),
            gateway::VoiceAudioState::Idle
        );
        assert!(gateway.voice_call.is_none());
        assert!(gateway.voice_guild.is_none());
    }
}

#[test]
fn proxy_password_ui_messages_are_redacted() {
    let message = Message::Network(network_ui::Action::ProxyPassword(Arc::new(UserToken::new(
        "proxy-canary-secret".into(),
    ))));
    assert!(!format!("{message:?}").contains("proxy-canary-secret"));
}

#[test]
fn a_configured_socks5_proxy_allows_an_explicit_call_intent() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    app.network = app
        .network
        .with_proxy(
            fastcord_net::ProxyConfig {
                kind: fastcord_net::ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                port: 1080,
                username: None,
            },
            zeroize::Zeroizing::new(String::new()),
        )
        .unwrap();
    drop(app.update(Message::StartCall(Snowflake(3))));
    assert!(!app.network_settings.open);
    let Some(gateway) = app.phase.gateway() else {
        panic!("account expected")
    };
    assert_eq!(gateway.voice_call, Some(Snowflake(3)));
    assert_eq!(
        gateway.controls.voice_audio_state(),
        gateway::VoiceAudioState::Connecting
    );
}

#[test]
fn settings_are_lazy_independent_and_preserve_pending_latest_save() {
    let mut app = login_app();
    assert!(!app.audio_settings.probing);
    let _ = app.update(Message::OpenAudioSettings);
    assert!(app.audio_settings.probing);
    let _ = app.update(Message::AudioDevicesListed(audio_devices::Inventory {
        input: Err("Input unavailable"),
        output: Ok(Vec::new()),
    }));
    assert!(!app.audio_settings.probing);
    assert_eq!(app.audio_settings.input_error, Some("Input unavailable"));
    assert_eq!(app.audio_settings.output_error, None);
    let option = |id: &str| audio_devices::DeviceOption {
        choice: fastcord_audio::DeviceChoice::Device(
            fastcord_audio::DeviceKey::from_persisted(id).unwrap(),
        ),
        label: "Headset".to_owned(),
        unavailable: false,
    };
    let _ = app.update(Message::SelectAudioDevice(
        fastcord_audio::Direction::Input,
        option("mic"),
    ));
    let first = app.audio_saving.clone().unwrap();
    let _ = app.update(Message::SelectAudioDevice(
        fastcord_audio::Direction::Output,
        option("headset"),
    ));
    assert_eq!(app.audio_saving, Some(first));
    let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
    assert_eq!(app.audio_saving.as_ref(), Some(&app.audio_preferences));
    let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
    assert!(app.audio_saving.is_none());
    let _ = app.view();
    let _ = app.update(Message::CloseAudioSettings);
    assert!(!app.audio_settings.open);
}

#[test]
fn noise_preferences_are_coalesced_restored_and_reselected_on_the_controls() {
    use fastcord_audio::NoiseSuppression::{HighQuality, Light, Off};
    let mut app = login_app();
    let _ = app.update(Message::NoiseSuppressionChanged(Off));
    assert_eq!(app.audio_preferences.noise_suppression, Off);
    assert_eq!(app.audio_saving.as_ref().unwrap().noise_suppression, Off);
    let _ = app.update(Message::NoiseSuppressionChanged(HighQuality));
    assert_eq!(app.audio_saving.as_ref().unwrap().noise_suppression, Off);
    let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
    assert_eq!(
        app.audio_saving.as_ref().unwrap().noise_suppression,
        HighQuality
    );
    let _ = app.update(Message::AudioPreferencesSaved(Ok(())));
    assert!(app.audio_saving.is_none());

    authenticate(&mut app, Persistence::MemoryOnly);
    let Some(gateway) = app.phase.gateway() else {
        panic!("not logged in");
    };
    let restored = gateway.controls.voice_controls();
    assert_eq!(restored.noise_suppression, HighQuality);
    let _ = app.update(Message::NoiseSuppressionChanged(Light));
    let _ = app.update(Message::NoiseSuppressionChanged(Light));
    let Some(gateway) = app.phase.gateway() else {
        panic!("not logged in");
    };
    let changed = gateway.controls.voice_controls();
    assert_eq!(changed.noise_suppression, Light);
    assert_eq!(
        changed.noise_generation,
        restored.noise_generation.wrapping_add(2)
    );
    assert_eq!(app.audio_preferences.noise_suppression, Light);
}

#[test]
fn the_audio_dialog_switches_the_level_meter_on_and_off_and_gain_reaches_the_call() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let diagnostics = |app: &App| match &app.phase {
        Phase::Account { gateway, .. } => gateway.controls.voice_controls().diagnostics_open,
        _ => panic!("not logged in"),
    };
    assert!(!diagnostics(&app), "closed: nothing is measured");
    // The 10 Hz timer exists only while the dialog is open.
    let _ = app.update(Message::OpenAudioSettings);
    assert!(diagnostics(&app));
    let _ = app.update(Message::MicPanel(mic_panel::Event::Gain(9)));
    let _ = app.update(Message::MicPanel(mic_panel::Event::Automatic(true)));
    let wanted = fastcord_audio::InputGain {
        manual_db: 9,
        automatic: true,
    };
    assert_eq!(app.audio_preferences.input_gain, wanted);
    let Some(gateway) = app.phase.gateway() else {
        panic!("not logged in");
    };
    assert_eq!(gateway.controls.voice_controls().input_gain, wanted);
    let _ = app.view();
    let _ = app.update(Message::CloseAudioSettings);
    assert!(!diagnostics(&app));
    assert!(!app.mic_panel.running());
}

#[test]
fn restored_device_ids_enter_the_call_controls_without_changing_mute_or_volumes() {
    let mut app = login_app();
    app.audio_preferences.input.choice = fastcord_audio::DeviceChoice::Device(
        fastcord_audio::DeviceKey::from_persisted("saved-microphone").unwrap(),
    );
    app.audio_preferences.output.choice = fastcord_audio::DeviceChoice::Device(
        fastcord_audio::DeviceKey::from_persisted("saved-headset").unwrap(),
    );
    authenticate(&mut app, Persistence::MemoryOnly);
    let Some(gateway) = app.phase.gateway() else {
        panic!("not logged in")
    };
    let restored = gateway.controls.voice_controls();
    assert_eq!(restored.input_device, app.audio_preferences.input.choice);
    assert_eq!(restored.output_device, app.audio_preferences.output.choice);
    gateway.controls.set_self_muted(true);
    gateway.controls.set_deafened(true);
    gateway.controls.set_volume_percent(Snowflake(99), 175);
    gateway.controls.select_audio_device(
        fastcord_audio::Direction::Output,
        fastcord_audio::DeviceChoice::SystemDefault,
    );
    let changed = gateway.controls.voice_controls();
    assert!(changed.self_muted && changed.deafened);
    assert_eq!(changed.volume_percent(Snowflake(99)), 175);
    assert_eq!(changed.input_device, restored.input_device);
    assert_eq!(
        changed.output_device,
        fastcord_audio::DeviceChoice::SystemDefault
    );
}

#[test]
fn password_input_messages_and_pending_sessions_are_redacted() {
    let message = token_edited("dummy-offline-secret".to_owned());
    assert!(!format!("{message:?}").contains("dummy-offline-secret"));
    let result = Message::LoginFinished(Ok(LoginOutcome::NeedsMemoryConsent(
        login::fixture_session(),
        StoreError::Locked,
    )));
    assert!(!format!("{result:?}").contains("dummy-offline-secret"));
    assert!(matches!(
        token_edited("x".repeat(login::MAX_TOKEN_BYTES + 1)),
        Message::InputTooLong
    ));
}

#[test]
fn submitting_moves_the_secret_and_trims_paste_whitespace() {
    let mut app = login_app();
    let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
    let _ = app.update(Message::TokenEdited(Arc::clone(&token)));
    let submitted = app.take_input_token();
    assert!(Arc::ptr_eq(&token, &submitted));
    assert!(app.token.expose_secret().is_empty());
    let _ = app.update(token_edited("  dummy-offline-secret \n".to_owned()));
    let submitted = app.take_input_token();
    assert!(submitted.expose_secret() == "dummy-offline-secret");
    assert!(app.token.expose_secret().is_empty());
}

#[test]
fn token_login_requires_warning_acknowledgement_and_bounded_nonempty_input() {
    let mut app = login_app();
    let _ = app.update(token_edited("dummy-offline-secret".into()));
    assert!(!app.can_login());
    let _ = app.update(Message::Login);
    assert!(matches!(app.phase, Phase::Login));
    let _ = app.update(Message::Acknowledge(true));
    assert!(app.can_login());
    let _ = app.update(Message::InputTooLong);
    assert!(app.token.expose_secret().is_empty());
    assert!(!app.can_login());
}

#[test]
fn invalid_token_stops_and_never_retains_an_authenticated_account() {
    let mut app = login_app();
    app.phase = Phase::Authenticating {
        saved_account: None,
    };
    let _ = app.update(Message::LoginFinished(Err(LoginError::Rest(
        fastcord_discord::RestError::AuthenticationRequired,
    ))));
    assert!(matches!(app.phase, Phase::Login));
    assert!(app.notice.as_deref().unwrap().contains("rejected"));
    assert!(app.saved_accounts.is_empty());
    assert!(app.token.expose_secret().is_empty());
}

#[test]
fn locked_store_does_not_silently_establish_a_memory_session() {
    let mut app = login_app();
    app.phase = Phase::Authenticating {
        saved_account: None,
    };
    let session = login::fixture_session();
    let _ = app.update(Message::LoginFinished(Ok(
        LoginOutcome::NeedsMemoryConsent(session, StoreError::Locked),
    )));
    assert!(matches!(app.phase, Phase::MemoryConsent { .. }));
    assert!(app.saved_accounts.is_empty());
    let _ = app.update(Message::ContinueInMemory);
    assert!(matches!(
        app.phase,
        Phase::Account {
            persistence: Persistence::MemoryOnly,
            ..
        }
    ));
    assert!(app.saved_accounts.is_empty());
}

#[tokio::test]
async fn logout_immediately_stops_workers_and_purges_account_and_input() {
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    let account = session.user.id;
    assert!(app.saved_accounts.contains(&account));
    let client = session.client.clone();
    drop(session);
    app.acknowledged = true;
    app.memory_only = true;
    let _ = app.update(Message::Logout);
    client.authentication_required().await;
    assert!(matches!(app.phase, Phase::Deleting { .. }));
    assert!(app.token.expose_secret().is_empty());
    assert!(!app.acknowledged && !app.memory_only);
    let _ = app.update(Message::Deleted(Err(StoreError::Locked)));
    assert!(matches!(app.phase, Phase::DeleteFailed { .. }));
    assert!(app.saved_accounts.contains(&account));
    let _ = app.update(Message::RetryDelete);
    let _ = app.update(Message::Deleted(Ok(())));
    assert!(matches!(app.phase, Phase::Login));
    assert!(app.saved_accounts.is_empty());
}

#[test]
fn memory_only_logout_does_not_create_a_saved_login_for_next_launch() {
    let mut app = login_app();
    drop(authenticate(&mut app, Persistence::MemoryOnly));
    assert!(app.saved_accounts.is_empty());
    let _ = app.update(Message::Logout);
    assert!(matches!(app.phase, Phase::Deleting { .. }));
    let _ = app.update(Message::Deleted(Ok(())));
    assert!(matches!(app.phase, Phase::Login));
    assert!(app.saved_accounts.is_empty());
    assert!(app.token.expose_secret().is_empty());
}

#[test]
fn cancelling_memory_consent_purges_the_validated_account() {
    let mut app = login_app();
    app.phase = Phase::MemoryConsent {
        session: login::fixture_session(),
        error: StoreError::Unavailable,
    };
    let _ = app.update(Message::CancelLogin);
    assert!(matches!(app.phase, Phase::Login));
    assert!(app.token.expose_secret().is_empty());
}

fn qr_link() -> fastcord_discord::QrLink {
    fastcord_discord::QrLink::new(
        "UZ0-kOVzXDZTFVV5_QlpURSO2BQHrtkKWHNpIGoDI0k",
        std::time::Duration::from_secs(300),
    )
}

fn qr_stage(app: &App) -> &QrStage {
    match &app.phase {
        Phase::Qr(attempt) => &attempt.stage,
        other => panic!("expected the QR screen, found {other:?}"),
    }
}

fn qr_id(app: &App) -> u64 {
    match &app.phase {
        Phase::Qr(attempt) => attempt.id,
        other => panic!("expected the QR screen, found {other:?}"),
    }
}

#[test]
fn qr_login_starts_only_after_the_warning_is_acknowledged() {
    let mut app = login_app();
    let _ = app.update(Message::StartQr);
    assert!(matches!(app.phase, Phase::Login));
    let _ = app.update(Message::Acknowledge(true));
    let _ = app.update(token_edited("dummy-offline-secret".into()));
    let _ = app.update(Message::StartQr);
    assert!(matches!(qr_stage(&app), QrStage::Connecting));
    // A half-typed token must not linger while another method runs.
    assert!(app.token.expose_secret().is_empty());
}

#[test]
fn qr_events_drive_the_screen_and_authorization_uses_the_token_path() {
    let mut app = login_app();
    app.acknowledged = true;
    let _ = app.update(Message::StartQr);
    let id = qr_id(&app);
    let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
    assert!(matches!(qr_stage(&app), QrStage::Showing { .. }));
    let user = fastcord_discord::RemoteUser {
        id: Snowflake(175_928_847_299_117_063),
        username: "alt_fixture".to_owned(),
        avatar: None,
    };
    let _ = app.update(Message::Qr(id, RemoteAuthEvent::PendingUser(user)));
    assert!(matches!(qr_stage(&app), QrStage::Scanned(_)));
    let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
    let _ = app.update(Message::Qr(id, RemoteAuthEvent::Authorized(token)));
    // Validation and storage happen in the same worker as token paste.
    assert!(matches!(
        app.phase,
        Phase::Authenticating {
            saved_account: None
        }
    ));
    assert!(app.token.expose_secret().is_empty());
}

#[test]
fn regenerating_uses_a_new_attempt_and_ignores_the_old_one() {
    let mut app = login_app();
    app.acknowledged = true;
    let _ = app.update(Message::StartQr);
    let first = qr_id(&app);
    let _ = app.update(Message::Qr(first, RemoteAuthEvent::Qr(qr_link())));
    let _ = app.update(Message::StartQr);
    let second = qr_id(&app);
    assert_ne!(first, second);
    assert!(matches!(qr_stage(&app), QrStage::Connecting));
    // Stragglers from the replaced attempt, even an authorization, do nothing.
    let _ = app.update(Message::Qr(first, RemoteAuthEvent::Qr(qr_link())));
    let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
    let _ = app.update(Message::Qr(first, RemoteAuthEvent::Authorized(token)));
    assert!(matches!(qr_stage(&app), QrStage::Connecting));
}

#[test]
fn cancel_returns_to_login_and_drops_the_attempt() {
    let mut app = login_app();
    app.acknowledged = true;
    let _ = app.update(Message::StartQr);
    let id = qr_id(&app);
    let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
    let _ = app.update(Message::CancelQr);
    assert!(matches!(app.phase, Phase::Login));
    assert!(app.acknowledged, "cancel keeps the acknowledgement");
    let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
    let _ = app.update(Message::Qr(id, RemoteAuthEvent::Authorized(token)));
    assert!(matches!(app.phase, Phase::Login));
}

#[test]
fn timeout_phone_cancel_and_captcha_end_the_attempt_with_clear_text() {
    use fastcord_discord::RemoteAuthError;
    for (error, regenerate) in [
        (RemoteAuthError::Expired, true),
        (RemoteAuthError::CancelledOnPhone, true),
        (RemoteAuthError::Captcha, false),
    ] {
        let mut app = login_app();
        app.acknowledged = true;
        let _ = app.update(Message::StartQr);
        let id = qr_id(&app);
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
        let _ = app.update(Message::Qr(id, RemoteAuthEvent::Failed(error)));
        assert!(matches!(qr_stage(&app), QrStage::Ended(e) if *e == error));
        assert_eq!(error.can_regenerate(), regenerate);
        // Regenerating from an ended attempt starts a clean one.
        if regenerate {
            let _ = app.update(Message::StartQr);
            assert!(matches!(qr_stage(&app), QrStage::Connecting));
        }
    }
    assert!(RemoteAuthError::Captcha.to_string().contains("token login"));
    assert!(
        RemoteAuthError::Captcha
            .to_string()
            .contains("official Discord client")
    );
}

#[test]
fn qr_messages_and_phases_never_print_codes_users_or_tokens() {
    let token = Arc::new(UserToken::new("dummy-offline-secret".to_owned()));
    let message = Message::Qr(1, RemoteAuthEvent::Authorized(token));
    assert!(!format!("{message:?}").contains("dummy-offline-secret"));
    let message = Message::Qr(1, RemoteAuthEvent::Qr(qr_link()));
    assert!(!format!("{message:?}").contains("UZ0-kOVz"));
    let mut app = login_app();
    app.acknowledged = true;
    let _ = app.update(Message::StartQr);
    let id = qr_id(&app);
    let _ = app.update(Message::Qr(id, RemoteAuthEvent::Qr(qr_link())));
    assert!(!format!("{:?}", app.phase).contains("UZ0-kOVz"));
}

fn gateway_status(app: &App) -> (u64, GatewayStatus) {
    match &app.phase {
        Phase::Account { gateway, .. } => (gateway.id, gateway.status.clone()),
        other => panic!("expected the account screen, found {other:?}"),
    }
}

#[test]
fn the_account_screen_owns_a_gateway_that_starts_connecting() {
    let mut app = login_app();
    drop(authenticate(&mut app, Persistence::Saved));
    let (id, status) = gateway_status(&app);
    assert_eq!(status, GatewayStatus::Connecting { attempt: 0 });
    assert!(status.describe().starts_with("Connecting to Discord"));
    // A second session gets a new identity.
    app.phase = Phase::Login;
    drop(authenticate(&mut app, Persistence::MemoryOnly));
    assert_ne!(gateway_status(&app).0, id);
}

#[test]
fn a_limit_stop_offers_reconnect_which_starts_a_fresh_gateway_and_other_stops_do_not() {
    use fastcord_discord::gateway::StopReason;
    let mut app = login_app();
    drop(authenticate(&mut app, Persistence::Saved));
    let (id, _) = gateway_status(&app);
    // A reason that reconnecting cannot fix offers nothing.
    let _ = app.update(Message::Gateway(
        id,
        GatewayStatus::Stopped(StopReason::EventTooLarge),
    ));
    let _ = app.update(Message::ReconnectGateway);
    assert_eq!(gateway_status(&app).0, id);
    // fastcord's own limits do: the screen stays, with a new Gateway.
    let _ = app.update(Message::Gateway(
        id,
        GatewayStatus::Stopped(StopReason::ReconnectFailures),
    ));
    assert!(gateway_status(&app).1.describe().contains("Ten connection"));
    // A draft in another conversation is kept across the reconnect.
    if let Some(gateway) = app.phase.gateway_mut() {
        gateway.composer.select(Some(Snowflake(41)));
        assert!(gateway.composer.restore("half-written"));
        gateway.composer.select(Some(Snowflake(42)));
        assert!(gateway.composer.has_draft(Snowflake(41)));
    }
    drop(app.update(Message::ReconnectGateway));
    let Some(gateway) = app.phase.gateway() else {
        panic!("account");
    };
    assert!(gateway.composer.has_draft(Snowflake(41)), "draft survives");
    let (new_id, status) = gateway_status(&app);
    assert_ne!(new_id, id);
    assert_eq!(status, GatewayStatus::Connecting { attempt: 0 });
    // Nothing to reconnect once connecting again.
    let _ = app.update(Message::ReconnectGateway);
    assert_eq!(gateway_status(&app).0, new_id);
}

#[test]
fn memory_only_continuation_also_starts_the_gateway() {
    let mut app = login_app();
    app.phase = Phase::MemoryConsent {
        session: login::fixture_session(),
        error: StoreError::Locked,
    };
    let _ = app.update(Message::ContinueInMemory);
    assert_eq!(
        gateway_status(&app).1,
        GatewayStatus::Connecting { attempt: 0 }
    );
}

#[test]
fn gateway_status_updates_apply_but_stale_sessions_are_ignored() {
    let mut app = login_app();
    drop(authenticate(&mut app, Persistence::Saved));
    let (id, _) = gateway_status(&app);
    let counts = gateway::Counts {
        guilds: 2,
        unavailable: 0,
        direct_messages: 1,
    };
    let _ = app.update(Message::Gateway(id, GatewayStatus::Ready(counts)));
    assert_eq!(gateway_status(&app).1, GatewayStatus::Ready(counts));
    // A straggler from an earlier session must not change the screen.
    let _ = app.update(Message::Gateway(id + 1, GatewayStatus::Resuming));
    assert_eq!(gateway_status(&app).1, GatewayStatus::Ready(counts));
}

#[tokio::test]
async fn a_rejected_token_stops_everything_and_returns_to_login() {
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    let client = session.client.clone();
    let account = session.user.id;
    drop(session);
    let (id, _) = gateway_status(&app);
    let _ = app.update(Message::Gateway(id, GatewayStatus::AuthenticationRequired));
    client.authentication_required().await;
    assert!(matches!(app.phase, Phase::Login));
    assert!(app.notice.as_deref().unwrap().contains("no longer accepts"));
    assert!(
        app.notice
            .as_deref()
            .unwrap()
            .contains("official Discord client")
    );
    assert!(app.notice.as_deref().unwrap().contains("password reset"));
    assert!(app.token.expose_secret().is_empty());
    // The saved credential is not silently deleted; the user decides.
    assert!(app.saved_accounts.contains(&account));
}

#[tokio::test]
async fn a_gateway_account_check_tears_down_panels_and_requests() {
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    let client = session.client.clone();
    let (id, _) = gateway_status(&app);
    let task = app.update(Message::NotifySettings(notify_settings::Event::Toggle));
    assert_eq!(task.units(), 0);
    assert!(app.ui.notify_settings.is_open());
    let _ = app.update(Message::Gateway(
        id,
        GatewayStatus::Stopped(fastcord_discord::gateway::StopReason::ActionRequired(
            "REQUIRE_VERIFIED_EMAIL".to_owned(),
        )),
    ));
    client.authentication_required().await;
    assert!(matches!(app.phase, Phase::Login));
    assert!(!app.ui.notify_settings.is_open());
    assert!(
        app.notice
            .as_deref()
            .unwrap()
            .contains("official Discord client")
    );
}

#[tokio::test]
async fn the_unverified_write_switch_is_session_only_and_resets_on_logout() {
    use fastcord_discord::WriteGroup;
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    let _ = app.update(Message::UnverifiedWrite(WriteGroup::Reports, true));
    let _ = app.update(Message::UnverifiedWrite(WriteGroup::Typing, false));
    assert!(session.client.write_group_enabled(WriteGroup::Reports));
    assert!(!session.client.write_group_enabled(WriteGroup::Typing));
    drop(session);
    let _ = app.update(Message::ToggleUnverifiedList);
    assert!(app.ui.unverified_open);
    let _ = app.update(Message::Logout);
    assert!(!app.ui.unverified_open);
    let _ = app.update(Message::Deleted(Ok(())));
    // The next sign-in has a new client with the defaults, not the old switch.
    let next = authenticate(&mut app, Persistence::MemoryOnly);
    assert!(!next.client.write_group_enabled(WriteGroup::Reports));
    assert!(next.client.write_group_enabled(WriteGroup::Typing));
}

#[tokio::test]
async fn the_allow_all_switch_turns_every_group_on_until_logout() {
    use fastcord_discord::WriteGroup;
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    assert!(!session.client.write_group_enabled(WriteGroup::Reports));
    let _ = app.update(Message::UnverifiedWriteAll(true));
    assert!(
        unverified::rows(Some(&session.client))
            .iter()
            .all(|(_, on)| *on)
    );
    let _ = app.update(Message::UnverifiedWriteAll(false));
    assert!(
        unverified::rows(Some(&session.client))
            .iter()
            .all(|(_, on)| !*on)
    );
    let _ = app.update(Message::UnverifiedWriteAll(true));
    drop(session);
    let _ = app.update(Message::Logout);
    let _ = app.update(Message::Deleted(Ok(())));
    // The next sign-in starts from the default groups, not from "allow all".
    let next = authenticate(&mut app, Persistence::MemoryOnly);
    assert!(
        unverified::rows(Some(&next.client))
            .iter()
            .all(|(group, on)| *on == group.default_on())
    );
    assert!(!next.client.write_group_enabled(WriteGroup::Reports));
}

#[tokio::test]
async fn logout_drops_the_gateway_with_the_account_screen() {
    let mut app = login_app();
    drop(authenticate(&mut app, Persistence::Saved));
    let (id, _) = gateway_status(&app);
    let _ = app.update(Message::Logout);
    assert!(matches!(app.phase, Phase::Deleting { .. }));
    // Late status from the closed connection changes nothing.
    let _ = app.update(Message::Gateway(id, GatewayStatus::Resuming));
    assert!(matches!(app.phase, Phase::Deleting { .. }));
}

#[tokio::test]
async fn switching_tears_down_the_account_before_the_other_one_signs_in() {
    // The Linux Secret Service client blocks on its own runtime, which may not
    // start inside this test's runtime.
    let Ok(store) = std::thread::spawn(NativeCredentialStore::new)
        .join()
        .unwrap()
    else {
        return; // no native store on this machine
    };
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    let account = session.user.id;
    let client = session.client.clone();
    drop(session);
    app.store = Some(store);
    let other = Snowflake(42);
    app.saved_accounts.push(other);
    let (id, _) = gateway_status(&app);
    // Switching to the active or an unsaved account does nothing.
    let _ = app.update(Message::SwitchAccount(account));
    let _ = app.update(Message::SwitchAccount(Snowflake(43)));
    assert!(matches!(app.phase, Phase::Account { .. }));
    let _ = app.update(Message::Account(account_panel::Event::Toggle));
    assert!(app.ui.account_panel.is_open());

    let _ = app.update(Message::SwitchAccount(other));
    // The old gateway and voice are gone, panels cleared, and the token
    // of the old account has stopped working before the new login starts.
    client.authentication_required().await;
    assert!(matches!(app.phase, Phase::Authenticating { saved_account: Some(a) } if a == other));
    assert!(!app.ui.account_panel.is_open());
    let _ = app.update(Message::Gateway(id, GatewayStatus::Resuming));
    assert!(matches!(app.phase, Phase::Authenticating { .. }));
    // Unlike logout, the old account's saved login stays.
    assert!(app.saved_accounts.contains(&account));
}

#[test]
fn a_memory_only_session_cannot_be_switched_away_from() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    app.saved_accounts.push(Snowflake(42));
    let _ = app.update(Message::SwitchAccount(Snowflake(42)));
    assert!(matches!(app.phase, Phase::Account { .. }));
}

#[test]
fn leaving_or_reentering_an_account_resets_every_panel_of_it() {
    // Before #169 two hand-written lists disagreed: the new-DM selection
    // and the open menu survived a switch, search hits a logout.
    let mut app = login_app();
    let session = authenticate(&mut app, Persistence::Saved);
    app.ui.new_dm.update(new_dm::Event::Open);
    app.ui.new_dm.update(new_dm::Event::Toggle(Snowflake(7)));
    app.ui.menu = Some(Menu::Conversation);
    app.ui.requests_open = true;
    assert_eq!(app.ui.new_dm.recipients(), "7");
    assert!(app.teardown_account().is_some());
    assert_eq!(app.ui.new_dm.recipients(), "");
    assert_eq!(app.ui.menu, None);
    assert!(!app.ui.requests_open);
    // Entering an account (a switch, or Reconnect) starts clean as well.
    app.ui.voice_details = true;
    let _ = app.enter_account(session, Persistence::Saved);
    assert!(!app.ui.voice_details);
}

#[test]
fn chords_open_overlays_and_escape_unwinds_them_before_cancelling_a_reply() {
    use switcher::Shortcut;
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let channel = Snowflake(500);
    let reply = outbox::Reply {
        message: Snowflake(7),
        author: "a".into(),
        content: "b".into(),
        mention_author: false,
    };
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    gateway.composer.select(Some(channel));
    assert!(gateway.composer.restore_send("", &[], Some(reply)));
    let _ = app.update(Message::Shortcut(Shortcut::Switcher));
    assert!(app.ui.switcher.is_open());
    let _ = app.update(Message::Shortcut(Shortcut::Help));
    assert!(app.ui.switcher.help_open() && !app.ui.switcher.is_open());
    // Escape closes the overlay first and leaves the reply alone.
    let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: false }));
    assert!(!app.ui.switcher.help_open());
    let replying = |app: &App| matches!(&app.phase, Phase::Account { gateway, .. } if gateway.composer.replying().is_some());
    assert!(replying(&app));
    // A focused composer already handled its own Escape.
    let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: true }));
    assert!(replying(&app));
    let _ = app.update(Message::Shortcut(Shortcut::Escape { captured: false }));
    assert!(!replying(&app));
    // Plain arrows do nothing while the switcher is closed.
    let _ = app.update(Message::Shortcut(Shortcut::Move(1)));
    assert!(!app.ui.switcher.is_open());
}

#[test]
fn escape_closes_the_topmost_layer_first_and_settings_before_sign_in() {
    use switcher::Shortcut;
    let escape = Message::Shortcut(Shortcut::Escape { captured: false });
    // Before sign-in the settings screen closes too.
    let mut app = login_app();
    let _ = app.update(Message::Settings(settings::Event::Open(
        settings::Section::Network,
    )));
    assert!(app.audio_settings.open && app.network_settings.open);
    let _ = app.update(escape.clone());
    assert!(!app.audio_settings.open && !app.network_settings.open);
    // Signed in: a menu over the settings goes first, then the screen.
    authenticate(&mut app, Persistence::MemoryOnly);
    let _ = app.update(Message::Settings(settings::Event::Open(
        settings::Section::Chat,
    )));
    app.mic_menu = true;
    let _ = app.update(escape.clone());
    assert!(!app.mic_menu && app.audio_settings.open);
    let _ = app.update(escape);
    assert!(!app.audio_settings.open);
}

#[test]
fn picking_a_switcher_result_closes_it_and_awaits_the_navigation() {
    use fastcord_discord::state::navigation::{NavigationSnapshot, QuickEntry, QuickKind};
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let entry = |id: u64, guild: Option<u64>, kind| QuickEntry {
        channel: Snowflake(id),
        guild: guild.map(Snowflake),
        guild_name: String::new(),
        name: "x".to_owned(),
        kind,
        unread: false,
        mentions: 0,
    };
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    gateway.navigation = Arc::new(NavigationSnapshot {
        quick: vec![
            entry(1, Some(9), QuickKind::Voice),
            entry(2, None, QuickKind::Direct),
        ],
        ..NavigationSnapshot::default()
    });
    for (index, directs) in [(0, false), (1, true)] {
        let _ = app.update(Message::Shortcut(switcher::Shortcut::Switcher));
        assert!(app.ui.switcher.is_open());
        app.ui.nav_pending = false;
        let _ = app.update(Message::Switcher(switcher::Event::Pick(index)));
        assert!(!app.ui.switcher.is_open());
        assert!(app.ui.nav_pending, "focus returns once the chat is open");
        let Some(gateway) = app.phase.gateway() else {
            panic!("account");
        };
        assert_eq!(gateway.directs, directs);
        assert!(
            gateway.controls.voice_target().is_none(),
            "a voice row opens its chat, never the call"
        );
    }
}

#[test]
fn gateway_messages_and_phases_never_print_secrets() {
    let mut app = login_app();
    drop(authenticate(&mut app, Persistence::Saved));
    let (id, _) = gateway_status(&app);
    let message = Message::Gateway(
        id,
        GatewayStatus::Stopped(fastcord_discord::gateway::StopReason::EventTooLarge),
    );
    assert!(!format!("{message:?}").contains("dummy-offline-secret"));
    assert!(!format!("{:?}", app.phase).contains("dummy-offline-secret"));
}
#[test]
fn private_recipient_selection_is_manual_bounded_and_unique() {
    let account = Snowflake(1);
    assert_eq!(
        parse_private_recipients(" 42, 43 ", account).unwrap(),
        [Snowflake(42), Snowflake(43)]
    );
    assert!(parse_private_recipients("", account).is_err());
    assert!(parse_private_recipients("42,42", account).is_err());
    assert!(parse_private_recipients("1", account).is_err());
    assert!(parse_private_recipients("not-an-id", account).is_err());
    assert!(
        parse_private_recipients(
            &(1..=MAX_PRIVATE_RECIPIENTS + 1)
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(","),
            account
        )
        .is_err()
    );
    assert!(
        parse_private_recipients(&"x".repeat(MAX_PRIVATE_RECIPIENT_INPUT_BYTES + 1), account)
            .is_err()
    );
}

#[test]
fn a_snapshot_carrying_the_last_status_still_looks_up_visible_invites() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    let (_, worker) = iced::Task::<()>::none().abortable();
    let id = gateway.id;
    *gateway = GatewayPanel::new(id, worker, gateway::NavigationBridge::new());
    let message: fastcord_model::Message = serde_json::from_value(serde_json::json!({
        "id": "42",
        "channel_id": "500",
        "author": {"id": "2", "username": "alt"},
        "content": "join https://discord.gg/abc123",
        "timestamp": "2026-10-24T12:00:00.000000+00:00",
    }))
    .unwrap();
    // Every snapshot after sign-in carries the last status; it must not
    // cut short the rest of the update.
    gateway.controls.publish(
        fastcord_discord::state::navigation::NavigationSnapshot::default(),
        timeline::Snapshot {
            channel_id: Some(Snowflake(500)),
            rows: vec![timeline::Row::new(Arc::new(message), 1)],
            ..timeline::Snapshot::default()
        },
        emoji_picker::Snapshot::default(),
        Some(GatewayStatus::SigningIn),
    );
    let _ = app.update(Message::Navigation(id));
    let Some(gateway) = app.phase.gateway() else {
        panic!("the status must not end the account");
    };
    assert_eq!(gateway.status, GatewayStatus::SigningIn);
    assert_eq!(
        app.ui.invite.cache.get("abc123").map(|entry| &entry.card),
        Some(&invite::Card::Loading),
        "the invite is looked up"
    );
}

#[test]
fn a_picker_opened_for_a_message_reacts_to_it_and_the_composers_button_inserts_text() {
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let channel = Snowflake(500);
    let thumbs = emoji_picker::Choice::Unicode(emoji::unicode("\u{1f44d}").unwrap());
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    // The account's own worker is not running in tests, so its command queue
    // is gone; a panel with a bridge whose receiver the test holds replaces it.
    let bridge = gateway::NavigationBridge::new();
    let mut commands = bridge.take_commands().unwrap();
    let (_, worker) = iced::Task::<()>::none().abortable();
    *gateway = GatewayPanel::new(gateway.id, worker, bridge);
    gateway.timeline = Arc::new(timeline::Snapshot {
        channel_id: Some(channel),
        can_send: true,
        can_react: true,
        can_tts: true,
        ..timeline::Snapshot::default()
    });
    gateway.composer.select(Some(channel));
    let page = |gateway: &GatewayPanel| {
        Arc::new(emoji_picker::Snapshot {
            revision: gateway.emoji_picker.request.revision,
            channel: Some(channel),
            offset: 0,
            total: 1,
            cells: vec![emoji_picker::Cell {
                choice: thumbs.clone(),
                guild_name: None,
                variants: None,
            }],
        })
    };

    // React on a message, then pick: a reaction request, not editor text.
    let _ = app.update(Message::Timeline(timeline::Event::React {
        channel_id: channel,
        message_id: Snowflake(42),
    }));
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    assert!(gateway.emoji_picker.request.open);
    gateway.emoji_snapshot = page(gateway);
    let _ = app.update(Message::EmojiPicker(emoji_picker::Event::Select(
        thumbs.clone(),
    )));
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    assert!(!gateway.emoji_picker.request.open);
    assert!(gateway.composer.submission(true, 0).is_none(), "no text");
    assert!(matches!(
        commands.try_recv(),
        Ok(gateway::Command::AddReaction { channel: c, message, emoji })
            if (c, message) == (channel, Snowflake(42))
                && emoji == thumbs.reaction_emoji()
    ));
    assert!(commands.try_recv().is_err());
    assert_eq!(gateway.emoji_picker.request.recent[0], thumbs);

    // The composer's button still inserts text.
    let _ = app.update(Message::EmojiPicker(emoji_picker::Event::Toggle));
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    assert!(gateway.emoji_picker.request.open);
    assert_eq!(gateway.emoji_picker.reaction(), None);
    gateway.emoji_snapshot = page(gateway);
    let _ = app.update(Message::EmojiPicker(emoji_picker::Event::Select(thumbs)));
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    assert!(gateway.composer.submission(true, 0).is_some(), "text");
    assert!(commands.try_recv().is_err(), "nothing was reacted to");
}

#[test]
fn media_subscription_ticks_only_while_audio_is_playing() {
    use fastcord_audio::PlayerState;
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    assert_eq!(app.media_subscription().units(), 0);
    let key = AttachmentKey {
        account: Snowflake(1),
        channel: Snowflake(2),
        message: Snowflake(3),
        attachment: Snowflake(4),
    };
    let set = |app: &mut App, phase, state| {
        let Some(gateway) = app.phase.gateway_mut() else {
            panic!("account");
        };
        let mut media = media_ui::MediaSession::for_test(key);
        media.phase = phase;
        media.snapshot.state = state;
        gateway.media = Some(media);
    };
    set(&mut app, media_ui::Phase::Loading, PlayerState::Playing);
    assert_eq!(app.media_subscription().units(), 0);
    set(&mut app, media_ui::Phase::Active, PlayerState::Playing);
    assert_eq!(app.media_subscription().units(), 1);
    set(&mut app, media_ui::Phase::Active, PlayerState::Paused);
    assert_eq!(app.media_subscription().units(), 0);
    set(&mut app, media_ui::Phase::Failed("x"), PlayerState::Playing);
    assert_eq!(app.media_subscription().units(), 0);
}

#[test]
fn video_subscription_tracks_pane_focus_resize_occlusion_and_unwatch() {
    use std::sync::atomic::Ordering;
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let bridge = gateway::NavigationBridge::new();
    let _commands = bridge.take_commands().unwrap();
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    let (_, worker) = iced::Task::<()>::none().abortable();
    *gateway = GatewayPanel::new(gateway.id, worker, bridge.clone());
    assert!(bridge.watch_stream("guild:41:127:99").is_ok());
    let watched = bridge.watched().pop().unwrap();
    assert_eq!(app.video_subscription().units(), 0);
    let _ = app.update(Message::ShowStream(true));
    assert_eq!(app.video_subscription().units(), 1);
    // Losing focus keeps the stream playing, like the official client.
    let _ = app.update(Message::WindowActive(false));
    assert_eq!(app.video_subscription().units(), 1);
    assert!(watched.visible.load(Ordering::Acquire));
    let _ = app.update(Message::WindowActive(true));
    assert_eq!(app.video_subscription().units(), 1);
    let _ = app.update(Message::ShowStream(false));
    assert_eq!(app.video_subscription().units(), 0);
    let _ = app.update(Message::ShowStream(true));
    let _ = app.update(Message::OpenAudioSettings);
    assert_eq!(app.video_subscription().units(), 0);
    let _ = app.update(Message::CloseAudioSettings);
    assert_eq!(app.video_subscription().units(), 1);
    let _ = app.update(Message::WindowSize(iced::Size::ZERO));
    assert_eq!(app.video_subscription().units(), 0);
    let _ = app.update(Message::WindowSize(iced::Size::new(640.0, 480.0)));
    assert_eq!(app.video_subscription().units(), 1);
    let _ = app.update(Message::UnwatchStream("guild:41:127:99".into()));
    assert_eq!(app.video_subscription().units(), 0);
    assert!(bridge.watched().is_empty());
    assert!(!watched.active.load(Ordering::Acquire));
    assert!(watched.frame().is_none());
}

#[test]
fn several_watched_streams_are_decoded_only_while_the_layout_draws_them() {
    use crate::call_layout::{Mode, TileId};
    use std::sync::atomic::Ordering;
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let bridge = gateway::NavigationBridge::new();
    let _commands = bridge.take_commands().unwrap();
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("account");
    };
    let (_, worker) = iced::Task::<()>::none().abortable();
    *gateway = GatewayPanel::new(gateway.id, worker, bridge.clone());
    let keys: Vec<String> = (1..=4).map(|n| format!("guild:41:127:{n}")).collect();
    for key in &keys {
        assert!(bridge.watch_stream(key.clone()).is_ok());
    }
    let visible = |bridge: &gateway::NavigationBridge| {
        bridge
            .watched()
            .iter()
            .filter(|session| session.visible.load(Ordering::Acquire))
            .count()
    };
    // Watching alone draws nothing: the call view is closed.
    let _ = app.update(Message::WindowActive(true));
    assert_eq!((visible(&bridge), app.video_subscription().units()), (0, 0));
    let _ = app.update(Message::ShowStream(true));
    assert_eq!((visible(&bridge), app.video_subscription().units()), (4, 4));
    // Focus: still all four (one large, three small); a full view draws one.
    let _ = app.update(Message::SetCallMode(Mode::Focus));
    assert_eq!(visible(&bridge), 4);
    let _ = app.update(Message::FullTile(Some(TileId::Stream(keys[2].clone()))));
    assert_eq!((visible(&bridge), app.video_subscription().units()), (1, 1));
    assert!(bridge.watched()[2].visible.load(Ordering::Acquire));
    let _ = app.update(Message::FullTile(None));
    assert_eq!(visible(&bridge), 4);
    // Minimizing and covering stop every decoder; one stream can leave alone.
    let _ = app.update(Message::WindowSize(iced::Size::ZERO));
    assert_eq!(visible(&bridge), 0);
    let _ = app.update(Message::WindowSize(iced::Size::new(640.0, 480.0)));
    let _ = app.update(Message::UnwatchStream(keys[0].clone()));
    assert_eq!((bridge.watched().len(), visible(&bridge)), (3, 3));
    // Closing the call view hides all; the sessions stay connected.
    let _ = app.update(Message::ShowStream(false));
    assert_eq!((bridge.watched().len(), visible(&bridge)), (3, 0));
    for key in &keys[1..] {
        let _ = app.update(Message::UnwatchStream(key.clone()));
    }
    assert!(bridge.watched().is_empty());
}

#[test]
fn a_voice_channels_chat_and_its_call_are_selected_independently() {
    use fastcord_discord::state::navigation::{
        ChannelKind, ChannelRow, NavigationSnapshot, PermissionSummary, Window,
    };
    let (guild, voice, other) = (Snowflake(10), Snowflake(11), Snowflake(12));
    let row = |id, connect| ChannelRow {
        id,
        name: "Lounge".to_owned(),
        kind: ChannelKind::Voice,
        parent_id: None,
        enabled: true,
        selected: false,
        permissions: PermissionSummary {
            view_channel: true,
            connect,
            ..PermissionSummary::default()
        },
        unread: false,
        mentions: 0,
        thread: None,
        stage: false,
        collapsed: false,
        occupant: None,
    };
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("not logged in");
    };
    gateway.navigation = Arc::new(NavigationSnapshot {
        selected_guild: Some(guild),
        channels: Window {
            offset: 0,
            total: 2,
            rows: vec![row(voice, true), row(other, false)],
        },
        ..NavigationSnapshot::default()
    });
    let state = |app: &App| match &app.phase {
        Phase::Account { gateway, .. } => {
            (gateway.voice_guild, gateway.controls.voice_audio_state())
        }
        _ => panic!("not logged in"),
    };
    // Opening a chat, even of the connectable channel, never joins.
    let _ = app.update(Message::SelectChannel(guild, voice));
    let _ = app.update(Message::SelectChannel(guild, other));
    assert_eq!(state(&app), (None, gateway::VoiceAudioState::Idle));
    // Joining a call does not touch the chat selection (the intent queue
    // holds the last SelectChannel), and a channel without CONNECT is refused.
    let _ = app.update(Message::JoinVoice(guild, other));
    assert_eq!(state(&app), (None, gateway::VoiceAudioState::Idle));
    let _ = app.update(Message::JoinVoice(guild, voice));
    assert_eq!(
        state(&app),
        (Some(guild), gateway::VoiceAudioState::Connecting)
    );
    // Switching the chat while connected neither leaves nor moves the call.
    let _ = app.update(Message::SelectChannel(guild, other));
    assert_eq!(
        state(&app),
        (Some(guild), gateway::VoiceAudioState::Connecting)
    );
}

#[test]
fn the_voice_panel_exists_only_in_a_call_and_sits_above_the_account_footer() {
    use crate::variable_list::harness::Ui;
    use fastcord_discord::state::navigation::{
        ChannelKind, ChannelRow, NavigationSnapshot, PermissionSummary, Window,
    };
    use std::collections::BTreeSet;
    let (guild, voice) = (Snowflake(10), Snowflake(11));
    let mut app = login_app();
    authenticate(&mut app, Persistence::MemoryOnly);
    let Some(gateway) = app.phase.gateway_mut() else {
        panic!("not logged in");
    };
    gateway.navigation = Arc::new(NavigationSnapshot {
        selected_guild: Some(guild),
        channels: Window {
            offset: 0,
            total: 1,
            rows: vec![ChannelRow {
                id: voice,
                name: "Lounge".to_owned(),
                kind: ChannelKind::Voice,
                parent_id: None,
                enabled: true,
                selected: false,
                permissions: PermissionSummary {
                    view_channel: true,
                    connect: true,
                    ..PermissionSummary::default()
                },
                unread: false,
                mentions: 0,
                thread: None,
                stage: false,
                collapsed: false,
                occupant: None,
            }],
        },
        ..NavigationSnapshot::default()
    });
    // Clicks over the bottom of the sidebar (rail is 72 px, sidebar 240 px).
    let bottom = |app: &App| -> BTreeSet<String> {
        let mut ui = Ui::new(1000.0, 700.0, app.view());
        let mut hit = BTreeSet::new();
        for y in (440..700).step_by(4) {
            for x in (72..312).step_by(4) {
                for message in ui.click(x as f32, y as f32) {
                    let name = format!("{message:?}");
                    hit.insert(name.split(['(', ' ']).next().unwrap().to_owned());
                }
            }
        }
        hit
    };
    let idle = bottom(&app);
    assert!(
        !idle.contains("OpenCall") && !idle.contains("LeaveVoice"),
        "{idle:?}"
    );
    // The footer's own actions are there all along: the account menu,
    // the device menu and the settings.
    for name in ["Status", "MicMenu", "Settings"] {
        assert!(idle.contains(name), "{name}: {idle:?}");
    }
    let _ = app.update(Message::JoinVoice(guild, voice));
    let joined = bottom(&app);
    for name in [
        "OpenCall",
        "LeaveVoice",
        "ToggleVoiceDetails",
        "Status",
        "Settings",
    ] {
        assert!(joined.contains(name), "{name}: {joined:?}");
    }
}

#[test]
fn closing_quits_by_default_and_hides_only_with_the_tray_chosen_and_up() {
    let mut app = login_app();
    assert!(!app.audio_preferences.close_to_tray, "default is quit");
    let _ = app.update(Message::CloseRequested);
    assert!(!app.window_hidden, "default: the window is not hidden");
    app.audio_preferences.close_to_tray = true;
    let _ = app.update(Message::CloseRequested);
    assert!(!app.window_hidden, "no tray icon, no hiding");
    let _ = app.update(Message::Tray(desktop::TrayEvent::Started));
    let _ = app.update(Message::CloseRequested);
    assert!(app.window_hidden && !app.window_drawable);
    // The tray's Open brings it back.
    let _ = app.update(Message::Tray(desktop::TrayEvent::Command(
        fastcord_platform::tray::Command::Open,
    )));
    assert!(!app.window_hidden && app.window_drawable);
}

#[test]
fn a_link_from_a_second_launch_waits_for_sign_in_and_is_opened_once() {
    use fastcord_platform::instance::Forwarded;
    let mut app = login_app();
    let _ = app.update(Message::Forwarded(Forwarded::Raise));
    assert_eq!(app.pending_link, None);
    let _ = app.update(Message::Forwarded(Forwarded::Open("not a link".into())));
    assert_eq!(app.pending_link, None);
    let _ = app.update(Message::Forwarded(Forwarded::Open(
        "discord://-/channels/1/2/3".into(),
    )));
    assert!(app.pending_link.is_some());
    authenticate(&mut app, Persistence::MemoryOnly);
    assert_eq!(app.pending_link, None, "taken once signed in");
}
