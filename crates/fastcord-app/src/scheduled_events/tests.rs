use super::*;
use fastcord_discord::state::navigation::{ChannelRow, PermissionSummary};
use fastcord_discord::{EventPermissions, NetworkFailure, RetryableFailure};

fn client() -> RestClient {
    RestClient::new(
        fastcord_net::NetworkContext::off(),
        fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
        fastcord_discord::gateway::ClientProperties::bundled(
            fastcord_discord::gateway::HostOs::Linux,
            "en-US",
        ),
    )
    .unwrap()
}

fn guild() -> Snowflake {
    Snowflake(100)
}

fn event(id: u64, status: EventStatus, kind: EventKind, start_ms: u64) -> ScheduledEvent {
    ScheduledEvent {
        id: Snowflake(id),
        guild_id: guild(),
        channel_id: (kind != EventKind::External).then_some(Snowflake(200)),
        creator_id: Some(Snowflake(9)),
        creator: "Host".into(),
        name: format!("Event {id}"),
        description: String::new(),
        start_ms,
        end_ms: None,
        status,
        kind,
        location: String::new(),
        user_count: Some(3),
        interested: None,
        recurrence: None,
    }
}

fn channel(connect: bool) -> ChannelRow {
    ChannelRow {
        id: Snowflake(200),
        name: "Lounge".into(),
        kind: ChannelKind::Voice,
        parent_id: None,
        enabled: true,
        selected: false,
        permissions: PermissionSummary {
            connect,
            ..PermissionSummary::default()
        },
        unread: false,
        mentions: 0,
        thread: None,
        occupant: None,
        stage: false,
        collapsed: false,
    }
}

fn nav(events: Vec<ScheduledEvent>, connect: bool) -> NavigationSnapshot {
    let mut nav = NavigationSnapshot {
        selected_guild: Some(guild()),
        events,
        event_permissions: EventPermissions {
            manage: true,
            create: true,
        },
        ..NavigationSnapshot::default()
    };
    nav.readable_events = nav.events.iter().map(|event| event.id).collect();
    nav.channels.rows = vec![channel(connect)];
    nav
}

fn open(state: &mut State, nav: &NavigationSnapshot, bridge: &NavigationBridge) {
    let _ = state.update(Event::Open(guild()), nav, &client(), bridge);
    let _ = state.update(
        Event::Loaded(state.generation, guild(), Ok(Vec::new())),
        nav,
        &client(),
        bridge,
    );
}

const FUTURE: u64 = 4_000_000_000_000;

#[test]
fn local_times_round_trip_in_the_offset_and_reject_nonsense() {
    let plus3 = UtcOffset::from_hms(3, 0, 0).unwrap();
    let ms = parse_local("2026-10-30 18:30", plus3).unwrap();
    assert_eq!(ms, 1_793_374_200_000);
    assert_eq!(format_local(ms, plus3), "2026-10-30 18:30");
    assert_eq!(format_local(ms, UtcOffset::UTC), "2026-10-30 15:30");
    assert_eq!(parse_local("2026-10-30T18:30", plus3), Some(ms));
    for bad in [
        "",
        "2026-10-30",
        "2026-13-01 10:00",
        "2026-02-30 10:00",
        "2026-10-30 25:00",
        "2026-10-30 10",
        "a b",
        "2026-10-30 10:00:00",
        "1960-01-01 00:00",
    ] {
        assert_eq!(parse_local(bad, plus3), None, "{bad}");
    }
}

#[test]
fn opening_loads_once_and_hands_the_list_to_the_reducer() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let mut commands = bridge.take_commands().unwrap();
    let nav = nav(Vec::new(), true);
    let _ = state.update(Event::Open(guild()), &nav, &client(), &bridge);
    assert!(matches!(state.load, Load::Loading));
    // Opening again while it loads changes nothing.
    let generation = state.generation;
    let _ = state.update(Event::Open(guild()), &nav, &client(), &bridge);
    assert!(matches!(state.load, Load::Loading));

    // A stale answer (another account) and one for another server are dropped.
    state.clear();
    let _ = state.update(Event::Open(guild()), &nav, &client(), &bridge);
    let _ = state.update(
        Event::Loaded(generation, guild(), Ok(Vec::new())),
        &nav,
        &client(),
        &bridge,
    );
    let _ = state.update(
        Event::Loaded(state.generation, Snowflake(7), Ok(Vec::new())),
        &nav,
        &client(),
        &bridge,
    );
    assert!(commands.try_recv().is_err());
    assert!(matches!(state.load, Load::Loading));

    let listed = vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)];
    let _ = state.update(
        Event::Loaded(state.generation, guild(), Ok(listed)),
        &nav,
        &client(),
        &bridge,
    );
    assert!(matches!(state.load, Load::Idle));
    assert!(matches!(
        commands.try_recv(),
        Ok(crate::gateway::Command::EventsLoaded { guild: g, events }) if g == guild() && events.len() == 1
    ));

    // A failed list says why and can be retried.
    let _ = state.update(Event::Reload, &nav, &client(), &bridge);
    let _ = state.update(
        Event::Loaded(state.generation, guild(), Err(RestError::PermissionDenied)),
        &nav,
        &client(),
        &bridge,
    );
    assert!(matches!(&state.load, Load::Failed(m) if m.contains("403")));
    assert!(State::is_rejected(&Event::Loaded(
        0,
        guild(),
        Err(RestError::AuthenticationRequired)
    )));
}

#[test]
fn one_write_at_a_time_and_destructive_ones_ask_first() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let nav = nav(
        vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)],
        true,
    );
    open(&mut state, &nav, &bridge);

    let _ = state.update(
        Event::Ask(Confirm::Delete, Snowflake(1)),
        &nav,
        &client(),
        &bridge,
    );
    assert_eq!(state.confirm, Some((Confirm::Delete, Snowflake(1))));
    let _ = state.update(Event::Keep, &nav, &client(), &bridge);
    assert!(state.confirm.is_none() && state.pending.is_none());

    let go = Event::Do(EventAction::Interested(Snowflake(1)));
    let _ = state.update(go.clone(), &nav, &client(), &bridge);
    let serial = state.pending.unwrap();
    let _ = state.update(go, &nav, &client(), &bridge);
    let _ = state.update(
        Event::Do(EventAction::Start(Snowflake(1))),
        &nav,
        &client(),
        &bridge,
    );
    assert_eq!(state.pending, Some(serial), "still the one request");
    // Closing and reopening the panel does not forget the request.
    let _ = state.update(Event::Close, &nav, &client(), &bridge);
    let _ = state.update(Event::Open(guild()), &nav, &client(), &bridge);
    assert_eq!(
        state.pending,
        Some(serial),
        "a reopen keeps the write in flight"
    );
    open(&mut state, &nav, &bridge);
    let _ = state.update(
        Event::Ask(Confirm::Cancel, Snowflake(1)),
        &nav,
        &client(),
        &bridge,
    );
    assert!(
        state.confirm.is_none(),
        "no question while a write is in flight"
    );

    // An answer to an older request changes nothing.
    let _ = state.update(
        Event::Done(serial.wrapping_sub(1), Ok(())),
        &nav,
        &client(),
        &bridge,
    );
    assert_eq!(state.pending, Some(serial));
    let _ = state.update(Event::Done(serial, Ok(())), &nav, &client(), &bridge);
    assert!(state.pending.is_none());
    assert!(state.notice.as_deref().unwrap().contains("confirms"));
}

#[test]
fn failures_are_shown_and_never_applied() {
    for (error, expect) in [
        (RestError::PermissionDenied, "403"),
        (RestError::ResourceGone, "could not find"),
        (
            RestError::Http(reqwest::StatusCode::TOO_MANY_REQUESTS),
            "rate limiting",
        ),
        (
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Timeout)),
            "may have been applied",
        ),
    ] {
        let (mut state, bridge) = (State::default(), NavigationBridge::new());
        let nav = nav(
            vec![event(1, EventStatus::Active, EventKind::Voice, FUTURE)],
            true,
        );
        open(&mut state, &nav, &bridge);
        let _ = state.update(
            Event::Do(EventAction::End(Snowflake(1))),
            &nav,
            &client(),
            &bridge,
        );
        let serial = state.pending.unwrap();
        let _ = state.update(Event::Done(serial, Err(error)), &nav, &client(), &bridge);
        let shown = state.notice.as_deref().unwrap();
        assert!(shown.contains(expect), "{shown}");
        assert!(state.pending.is_none());
    }
    assert!(State::is_rejected(&Event::Done(
        0,
        Err(RestError::AuthenticationRequired)
    )));

    // A late answer after logout is ignored.
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let nav = nav(Vec::new(), true);
    open(&mut state, &nav, &bridge);
    let _ = state.update(
        Event::Do(EventAction::Delete(Snowflake(1))),
        &nav,
        &client(),
        &bridge,
    );
    let serial = state.pending.unwrap();
    state.clear();
    let _ = state.update(
        Event::Done(serial, Err(RestError::PermissionDenied)),
        &nav,
        &client(),
        &bridge,
    );
    assert!(state.notice.is_none());
}

// Deliver the deterministic worker result without polling its write task.
fn validate_save(state: &mut State, nav: &NavigationSnapshot, bridge: &NavigationBridge) {
    if state.pending.is_some() {
        let _ = state.update(Event::Save, nav, &client(), bridge);
        return;
    }
    let Screen::Form(form) = &state.screen else {
        panic!("form");
    };
    let form = (**form).clone();
    let guild = state.guild.unwrap();
    let _ = state.update(Event::Save, nav, &client(), bridge);
    let serial = state.pending.unwrap();
    let result = draft_of(&form).map(|draft| match form.editing {
        Some((id, _)) => EventAction::Edit(id, draft),
        None => EventAction::Create(draft),
    });
    let _ = state.update(
        Event::Prepared(serial, guild, result),
        nav,
        &client(),
        bridge,
    );
}

#[test]
fn stale_timezone_validation_cannot_send_after_close_or_account_reset() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let nav = nav(Vec::new(), true);
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::New, &nav, &client(), &bridge);
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    let serial = state.pending.unwrap();
    let guild = state.guild.unwrap();
    let _ = state.update(Event::Close, &nav, &client(), &bridge);
    let _ = state.update(
        Event::Prepared(serial, guild, Err("stale".into())),
        &nav,
        &client(),
        &bridge,
    );
    assert!(state.pending.is_none());
    assert!(state.guild.is_none());
    assert!(state.notice.is_none());
    state.clear();
    let _ = state.update(
        Event::Prepared(serial, guild, Err("stale".into())),
        &nav,
        &client(),
        &bridge,
    );
    assert!(state.pending.is_none());
}

#[test]
fn the_form_is_checked_before_anything_is_sent() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let nav = nav(Vec::new(), true);
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::New, &nav, &client(), &bridge);
    let type_in = |state: &mut State, field, value: &str| {
        let _ = state.update(
            Event::Input(field, value.to_owned()),
            &NavigationSnapshot::default(),
            &client(),
            &NavigationBridge::new(),
        );
    };
    let error = |state: &State| match &state.screen {
        Screen::Form(form) => form.error.clone(),
        _ => panic!("no form"),
    };

    validate_save(&mut state, &nav, &bridge);
    assert!(error(&state).unwrap().contains("YYYY-MM-DD"));
    type_in(&mut state, Field::Start, "2020-01-01 10:00");
    type_in(&mut state, Field::Name, "Movie night");
    validate_save(&mut state, &nav, &bridge);
    assert!(
        error(&state).unwrap().contains("channel"),
        "{:?}",
        error(&state)
    );
    let _ = state.update(
        Event::Channel(Choice {
            id: Snowflake(200),
            name: "Lounge".into(),
        }),
        &nav,
        &client(),
        &bridge,
    );
    validate_save(&mut state, &nav, &bridge);
    assert!(error(&state).unwrap().contains("future"));
    assert!(state.pending.is_none());

    // An external event needs a location and an end.
    let _ = state.update(Event::Kind(EventKind::External), &nav, &client(), &bridge);
    type_in(&mut state, Field::Start, "2099-01-01 10:00");
    validate_save(&mut state, &nav, &bridge);
    assert!(error(&state).unwrap().contains("location"));
    type_in(&mut state, Field::Location, "Town hall");
    validate_save(&mut state, &nav, &bridge);
    assert!(error(&state).unwrap().contains("end time"));
    type_in(&mut state, Field::End, "2099-01-01 09:00");
    validate_save(&mut state, &nav, &bridge);
    assert!(error(&state).unwrap().contains("after the start"));
    type_in(&mut state, Field::End, "2099-01-01 12:00");
    validate_save(&mut state, &nav, &bridge);
    assert!(state.pending.is_some() && error(&state).is_none());
    // A second click while it is sent does nothing; success returns to the list.
    let serial = state.pending.unwrap();
    validate_save(&mut state, &nav, &bridge);
    assert_eq!(state.pending, Some(serial));
    let _ = state.update(Event::Done(serial, Ok(())), &nav, &client(), &bridge);
    assert!(matches!(state.screen, Screen::List));

    // A failure keeps the form and its text.
    let _ = state.update(Event::New, &nav, &client(), &bridge);
    type_in(&mut state, Field::Name, "x");
    type_in(&mut state, Field::Start, "2099-01-01 10:00");
    let _ = state.update(
        Event::Channel(Choice {
            id: Snowflake(200),
            name: "Lounge".into(),
        }),
        &nav,
        &client(),
        &bridge,
    );
    validate_save(&mut state, &nav, &bridge);
    let serial = state.pending.unwrap();
    let _ = state.update(
        Event::Done(serial, Err(RestError::PermissionDenied)),
        &nav,
        &client(),
        &bridge,
    );
    assert!(error(&state).unwrap().contains("403"));
}

#[test]
fn editing_starts_from_the_event_and_an_unmoved_start_may_be_in_the_past() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let past = event(1, EventStatus::Active, EventKind::Voice, 1_000_000_020_000);
    let nav = nav(
        vec![
            past,
            event(2, EventStatus::Completed, EventKind::Voice, FUTURE),
        ],
        true,
    );
    TimeCache::new().prepare(&nav.events);
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::Edit(Snowflake(2)), &nav, &client(), &bridge);
    assert!(
        matches!(state.screen, Screen::List),
        "an ended event is not edited"
    );
    let _ = state.update(Event::Edit(Snowflake(1)), &nav, &client(), &bridge);
    validate_save(&mut state, &nav, &bridge);
    assert!(state.pending.is_some(), "the running event keeps its start");
}

#[test]
fn the_banner_names_only_events_that_can_be_joined_soon() {
    let now = now_ms();
    let state = State::default();
    let pick = |events: Vec<ScheduledEvent>, state: &State| {
        soon(state, &nav(events, true), now).map(|e| e.id.0)
    };
    let mk = |id, status, kind, start| event(id, status, kind, start);
    assert_eq!(
        pick(
            vec![mk(
                1,
                EventStatus::Scheduled,
                EventKind::Voice,
                now + SOON_MS - 1_000
            )],
            &state
        ),
        Some(1)
    );
    assert_eq!(
        pick(
            vec![mk(
                1,
                EventStatus::Scheduled,
                EventKind::Stage,
                now + SOON_MS + 60_000
            )],
            &state
        ),
        None,
        "too far"
    );
    assert_eq!(
        pick(
            vec![mk(
                1,
                EventStatus::Scheduled,
                EventKind::Voice,
                now - LATE_MS - 60_000
            )],
            &state
        ),
        None,
        "long overdue"
    );
    assert_eq!(
        pick(
            vec![mk(1, EventStatus::Scheduled, EventKind::External, now)],
            &state
        ),
        None,
        "nothing to join"
    );
    assert_eq!(
        pick(
            vec![mk(1, EventStatus::Canceled, EventKind::Voice, now)],
            &state
        ),
        None
    );
    assert_eq!(
        pick(
            vec![mk(
                1,
                EventStatus::Active,
                EventKind::Voice,
                now - 3_600_000
            )],
            &state
        ),
        Some(1)
    );

    let mut hidden = State::default();
    let (bridge, nav) = (
        NavigationBridge::new(),
        nav(
            vec![mk(1, EventStatus::Active, EventKind::Voice, now)],
            true,
        ),
    );
    let _ = hidden.update(Event::Hide(Snowflake(1)), &nav, &client(), &bridge);
    assert_eq!(soon(&hidden, &nav, now), None);
    for n in 0..(MAX_HIDDEN as u64 + 5) {
        let _ = hidden.update(Event::Hide(Snowflake(n)), &nav, &client(), &bridge);
    }
    assert_eq!(hidden.hidden.len(), MAX_HIDDEN);
}

#[test]
fn joining_needs_a_live_event_and_a_channel_the_account_may_connect_to() {
    let live = vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)];
    assert!(joinable(&nav(live.clone(), true), guild(), Snowflake(200)));
    assert!(
        !joinable(&nav(live.clone(), false), guild(), Snowflake(200)),
        "no CONNECT"
    );
    assert!(
        !joinable(&nav(live.clone(), true), guild(), Snowflake(201)),
        "another channel"
    );
    assert!(
        !joinable(&nav(live, true), Snowflake(7), Snowflake(200)),
        "another server"
    );
    let gone = vec![event(1, EventStatus::Canceled, EventKind::Voice, FUTURE)];
    assert!(
        !joinable(&nav(gone, true), guild(), Snowflake(200)),
        "a canceled event"
    );
    assert!(
        !joinable(&nav(Vec::new(), true), guild(), Snowflake(200)),
        "a deleted event"
    );
}

#[test]
fn every_screen_builds() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let mut running = event(2, EventStatus::Active, EventKind::External, FUTURE);
    running.location = "Town hall".into();
    running.end_ms = Some(FUTURE + 1);
    running.recurrence = Some("Every week".into());
    let nav = nav(
        vec![
            event(1, EventStatus::Scheduled, EventKind::Stage, FUTURE),
            running,
        ],
        true,
    );
    open(&mut state, &nav, &bridge);
    assert!(view(&state, &nav).is_some());
    for id in [1, 2, 3] {
        let _ = state.update(Event::Show(Snowflake(id)), &nav, &client(), &bridge);
        let _ = state.update(
            Event::Ask(Confirm::Cancel, Snowflake(id)),
            &nav,
            &client(),
            &bridge,
        );
        assert!(view(&state, &nav).is_some());
    }
    let _ = state.update(Event::New, &nav, &client(), &bridge);
    for kind in [EventKind::Stage, EventKind::External] {
        let _ = state.update(Event::Kind(kind), &nav, &client(), &bridge);
        assert!(view(&state, &nav).is_some());
    }
    let _ = state.update(Event::Close, &nav, &client(), &bridge);
    assert!(state.guild.is_none());
}

#[cfg(unix)]
#[test]
fn editing_keeps_the_known_instant_of_an_unchanged_folded_end() {
    if std::env::var_os("FASTCORD_EVENT_FOLD_FIXTURE").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "scheduled_events::tests::editing_keeps_the_known_instant_of_an_unchanged_folded_end", "--nocapture"])
            .env("FASTCORD_EVENT_FOLD_FIXTURE", "1")
            .env("TZ", "EST5EDT,M3.2.0/2,M11.1.0/2")
            .status().unwrap();
        assert!(status.success());
        return;
    }
    // 06:30 UTC is the second occurrence of 01:30 in this fixture zone.
    let end = parse_local("2026-11-01 06:30", UtcOffset::UTC).unwrap();
    let mut before = event(1, EventStatus::Active, EventKind::Voice, end - 7_200_000);
    before.end_ms = Some(end);
    let nav = nav(vec![before], true);
    TimeCache::new().prepare(&nav.events);
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::Edit(Snowflake(1)), &nav, &client(), &bridge);
    let Screen::Form(form) = &mut state.screen else {
        panic!("edit form")
    };
    assert!(
        system_time(&form.end).is_err(),
        "new ambiguous input is rejected"
    );
    form.name = "Renamed event".into();
    assert_eq!(draft_of(form).unwrap().end_ms, Some(end));
}

fn users_page() -> fastcord_discord::EventUsers {
    fastcord_discord::EventUsers {
        rows: vec![fastcord_discord::EventUser {
            id: Snowflake(8),
            name: "Member".into(),
        }],
        next: Some(Snowflake(8)),
    }
}

#[test]
fn attendee_pages_ignore_close_reset_and_permission_loss_and_do_not_accumulate() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let mut nav = nav(
        vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)],
        true,
    );
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
    let epoch = state.users_epoch;
    let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
    assert_eq!(state.users_epoch, epoch, "one read per click while idle");
    let _ = state.update(
        Event::UsersLoaded(epoch, Snowflake(1), Ok(users_page())),
        &nav,
        &client(),
        &bridge,
    );
    assert_eq!(state.users.as_ref().unwrap().rows.len(), 1);
    let _ = state.update(Event::NextUsers, &nav, &client(), &bridge);
    assert!(
        state.users.is_none() && state.users_loading,
        "old page is released"
    );
    let next = state.users_epoch;
    let _ = state.update(
        Event::UsersLoaded(epoch, Snowflake(1), Ok(users_page())),
        &nav,
        &client(),
        &bridge,
    );
    assert!(state.users.is_none() && state.users_loading);
    nav.readable_events.clear();
    state.sync_users(&nav);
    let _ = state.update(
        Event::UsersLoaded(next, Snowflake(1), Ok(users_page())),
        &nav,
        &client(),
        &bridge,
    );
    assert!(state.users.is_none() && !state.users_loading);
    nav.readable_events.push(Snowflake(1));
    for reset in [false, true] {
        let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
        let epoch = state.users_epoch;
        if reset {
            state.clear();
        } else {
            let _ = state.update(Event::Close, &nav, &client(), &bridge);
        }
        let _ = state.update(
            Event::UsersLoaded(epoch, Snowflake(1), Ok(users_page())),
            &nav,
            &client(),
            &bridge,
        );
        assert!(state.users.is_none() && !state.users_loading);
        open(&mut state, &nav, &bridge);
    }
    let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
    nav.events.clear();
    state.sync_users(&nav);
    assert!(state.users.is_none() && !state.users_loading);
}

#[test]
fn attendee_read_errors_release_loading_and_allow_only_explicit_retry() {
    for error in [
        RestError::PermissionDenied,
        RestError::ResourceGone,
        RestError::RateLimited,
    ] {
        let (mut state, bridge) = (State::default(), NavigationBridge::new());
        let nav = nav(
            vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)],
            true,
        );
        open(&mut state, &nav, &bridge);
        let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
        let epoch = state.users_epoch;
        let _ = state.update(
            Event::UsersLoaded(epoch, Snowflake(1), Err(error)),
            &nav,
            &client(),
            &bridge,
        );
        assert!(state.users_error.is_some() && !state.users_loading && state.users.is_none());
        assert_eq!(state.users_epoch, epoch, "no automatic retry");
        let _ = state.update(Event::FirstUsers, &nav, &client(), &bridge);
        assert!(state.users_loading);
    }
    assert!(State::is_rejected(&Event::UsersLoaded(
        0,
        Snowflake(1),
        Err(RestError::AuthenticationRequired)
    )));
}

#[test]
fn leaving_or_losing_access_aborts_the_actual_attendee_task_before_reopening() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let mut nav = nav(
        vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)],
        true,
    );
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
    let first = state.users_task.as_ref().unwrap().clone();
    let _ = state.update(Event::Show(Snowflake(1)), &nav, &client(), &bridge);
    assert!(
        first.is_aborted(),
        "Back cancels the HTTP future, not only its reply"
    );
    let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
    let second = state.users_task.as_ref().unwrap().clone();
    assert!(!second.is_aborted());
    nav.readable_events.clear();
    state.sync_users(&nav);
    assert!(second.is_aborted());
    nav.readable_events.push(Snowflake(1));
    let _ = state.update(Event::ShowUsers(Snowflake(1)), &nav, &client(), &bridge);
    let third = state.users_task.as_ref().unwrap().clone();
    state.clear();
    assert!(third.is_aborted());
}

#[test]
fn recurrence_creation_is_explicit_and_never_changes_an_existing_series() {
    let (mut state, bridge) = (State::default(), NavigationBridge::new());
    let nav = nav(vec![], true);
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::New, &nav, &client(), &bridge);
    let _ = state.update(
        Event::Repeat(Some(EventRecurrence::EveryOtherWeek)),
        &nav,
        &client(),
        &bridge,
    );
    let Screen::Form(form) = &mut state.screen else {
        panic!("new form")
    };
    form.name = "Weekly meeting".into();
    form.channel = Some(Snowflake(200));
    form.start = "2040-01-01 12:00".into();
    assert_eq!(
        draft_of(form).unwrap().recurrence,
        Some(EventRecurrence::EveryOtherWeek)
    );
    form.editing = Some((Snowflake(1), FUTURE));
    let _ = state.update(
        Event::Repeat(Some(EventRecurrence::Weekly)),
        &nav,
        &client(),
        &bridge,
    );
    let Screen::Form(form) = &state.screen else {
        panic!("edit form")
    };
    assert_eq!(
        form.recurrence,
        Some(EventRecurrence::EveryOtherWeek),
        "existing series ignores recurrence controls"
    );
    assert_eq!(
        draft_of(form).unwrap().recurrence,
        None,
        "existing series never sends rule changes"
    );
}

#[test]
fn real_event_widget_tree_survives_detail_and_attendee_page_transitions() {
    use crate::variable_list::harness::Ui;
    let nav = nav(
        vec![event(1, EventStatus::Scheduled, EventKind::Voice, FUTURE)],
        true,
    );
    let make = |screen| State {
        guild: Some(guild()),
        screen,
        ..State::default()
    };
    let list = make(Screen::List);
    let detail = make(Screen::Detail(Snowflake(1)));
    let loading = State {
        users_loading: true,
        ..make(Screen::Users(Snowflake(1)))
    };
    let page = |count, epoch| State {
        users_epoch: epoch,
        users: Some(fastcord_discord::EventUsers {
            rows: (0..count)
                .map(|n| fastcord_discord::EventUser {
                    id: Snowflake(40000 + n),
                    name: format!("Interested member {n:02}"),
                })
                .collect(),
            next: (count == 50).then_some(Snowflake(40050)),
        }),
        ..make(Screen::Users(Snowflake(1)))
    };
    let first = page(50, 1);
    let last = page(5, 2);
    let mut form = make(Screen::List);
    let bridge = NavigationBridge::new();
    let _ = form.update(Event::New, &nav, &client(), &bridge);
    let mut ui = Ui::new(600.0, 430.0, view(&list, &nav).unwrap());
    ui.draw();
    for state in [&detail, &loading, &first, &loading, &last, &form, &detail] {
        ui.show(view(state, &nav).unwrap());
        let _ = ui.redraw();
        ui.draw();
    }
}

#[test]
fn time_offsets_refresh_on_deadline_recover_missing_values_and_bound_work() {
    use std::cell::Cell;
    let now = std::time::Instant::now();
    let calls = Cell::new(0);
    let batches = Cell::new(0);
    let mut cache = TimeCache::isolated();
    assert!(cache.wait(now).is_none());
    assert!(cache.update_with(
        vec![1, 2, 2],
        now,
        || batches.set(batches.get() + 1),
        |ms| {
            calls.set(calls.get() + 1);
            (ms == 1).then_some(3600)
        }
    ));
    assert_eq!((calls.get(), batches.get()), (2, 1));
    assert_eq!(cache.wait(now), Some(TimeCache::INTERVAL));
    assert!(!cache.update_with(
        vec![1, 2],
        now + std::time::Duration::from_secs(59),
        || panic!("same interval must not refresh native timezone"),
        |_| panic!("cached None must also avoid repeated lookup")
    ));
    assert_eq!(
        cache.wait(now + std::time::Duration::from_secs(59)),
        Some(std::time::Duration::from_secs(1))
    );
    assert!(cache.update_with(
        vec![1, 2],
        now + TimeCache::INTERVAL,
        || batches.set(batches.get() + 1),
        |_| {
            calls.set(calls.get() + 1);
            Some(7200)
        }
    ));
    assert_eq!(cache.entries, vec![(1, Some(7200)), (2, Some(7200))]);
    assert_eq!((calls.get(), batches.get()), (4, 2));
    assert!(
        !cache.update_with(
            vec![1, 2],
            now + TimeCache::INTERVAL * 2,
            || {},
            |_| Some(7200)
        ),
        "quiet timer must not publish unchanged offsets"
    );
    calls.set(0);
    cache.update_with(
        (0..1000).collect(),
        now + TimeCache::INTERVAL * 3,
        || {},
        |_| {
            calls.set(calls.get() + 1);
            Some(0)
        },
    );
    assert_eq!(cache.entries.len(), 200);
    assert_eq!(calls.get(), 200);
    assert!(cache.update_with(
        vec![],
        now + TimeCache::INTERVAL * 3,
        || panic!("empty"),
        |_| panic!("empty")
    ));
    assert!(
        cache.wait(now).is_none(),
        "empty calendar disarms the worker timer"
    );
    assert!(!cache.refresh_due(now + TimeCache::INTERVAL * 4));
}

#[test]
fn editing_keeps_displayed_instants_when_current_timezone_differs() {
    let start = parse_local("2030-01-02 12:00", UtcOffset::UTC).unwrap();
    let end = start + 3_600_000;
    let current = fastcord_platform::timezone::offset_at((start / 1000) as i64).unwrap_or(0);
    let previous = UtcOffset::from_whole_seconds(if current == 0 { 3600 } else { 0 }).unwrap();
    let start_text = format_local(start, previous);
    let end_text = format_local(end, previous);
    let form = Form {
        editing: Some((Snowflake(1), start)),
        original_end: Some(end),
        original_civil: Some((start_text.clone(), end_text.clone())),
        recurrence: None,
        name: "Renamed".into(),
        description: String::new(),
        kind: EventKind::Voice,
        channel: Some(Snowflake(200)),
        location: String::new(),
        start: start_text,
        end: end_text,
        error: None,
    };
    let draft = draft_of(&form).unwrap();
    assert_eq!(draft.start_ms, start);
    assert_eq!(draft.end_ms, Some(end));
}

#[test]
fn time_offset_publication_rejects_late_old_owner_and_clears_for_empty_new_account() {
    let published = std::sync::Arc::new(std::sync::Mutex::new(PublishedOffsets::default()));
    let now = std::time::Instant::now();
    let mut old = TimeCache::with_published(published.clone());
    old.update_with(vec![1], now, || {}, |_| Some(3600));
    assert_eq!(published.lock().unwrap().entries, vec![(1, Some(3600))]);
    let mut current = TimeCache::with_published(published.clone());
    assert!(
        published.lock().unwrap().entries.is_empty(),
        "even an empty new account clears old times"
    );
    current.update_with(vec![2], now, || {}, |_| Some(7200));
    // An aborted old worker may still finish synchronous native lookup after
    // the new account published. Its owner check must reject that late write.
    old.update_with(vec![1], now + TimeCache::INTERVAL, || {}, |_| Some(10800));
    assert_eq!(published.lock().unwrap().entries, vec![(2, Some(7200))]);
    assert!(!current.update_with(vec![2], now + TimeCache::INTERVAL, || {}, |_| Some(7200)));
    assert_eq!(published.lock().unwrap().entries, vec![(2, Some(7200))]);
    let empty = TimeCache::with_published(published.clone());
    assert!(empty.wait(now).is_none());
    old.update_with(vec![1], now + TimeCache::INTERVAL * 2, || {}, |_| Some(0));
    assert!(published.lock().unwrap().entries.is_empty());
}
