use super::*;
use fastcord_discord::state::navigation::{ChannelRow, PermissionSummary};
use fastcord_discord::{EventPermissions, NetworkFailure, RetryableFailure};

fn client() -> RestClient {
    RestClient::new(
        fastcord_discord::UserToken::new("offline-test-credential".to_owned()),
        fastcord_discord::gateway::ClientProperties::web(
            fastcord_discord::gateway::HostOs::Linux,
            "en-US",
            1,
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
            RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection)),
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

    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    assert!(error(&state).unwrap().contains("YYYY-MM-DD"));
    type_in(&mut state, Field::Start, "2020-01-01 10:00");
    type_in(&mut state, Field::Name, "Movie night");
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
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
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    assert!(error(&state).unwrap().contains("future"));
    assert!(state.pending.is_none());

    // An external event needs a location and an end.
    let _ = state.update(Event::Kind(EventKind::External), &nav, &client(), &bridge);
    type_in(&mut state, Field::Start, "2099-01-01 10:00");
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    assert!(error(&state).unwrap().contains("location"));
    type_in(&mut state, Field::Location, "Town hall");
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    assert!(error(&state).unwrap().contains("end time"));
    type_in(&mut state, Field::End, "2099-01-01 09:00");
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    assert!(error(&state).unwrap().contains("after the start"));
    type_in(&mut state, Field::End, "2099-01-01 12:00");
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
    assert!(state.pending.is_some() && error(&state).is_none());
    // A second click while it is sent does nothing; success returns to the list.
    let serial = state.pending.unwrap();
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
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
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
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
    open(&mut state, &nav, &bridge);
    let _ = state.update(Event::Edit(Snowflake(2)), &nav, &client(), &bridge);
    assert!(
        matches!(state.screen, Screen::List),
        "an ended event is not edited"
    );
    let _ = state.update(Event::Edit(Snowflake(1)), &nav, &client(), &bridge);
    let _ = state.update(Event::Save, &nav, &client(), &bridge);
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
