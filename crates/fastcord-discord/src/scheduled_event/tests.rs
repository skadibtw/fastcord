use super::*;
use crate::gateway::{Dispatch, GatewayEvent, decode_fixture};
use crate::state::Store;
use serde_json::json;

const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
const LIST: &str = include_str!("../../../../fixtures/rest/scheduled-events-synthetic.json");
const ME: u64 = 175_928_847_299_117_063;
const GUILD: u64 = 41_771_983_423_143_937;
const CHANNEL: u64 = 41_771_983_423_143_939;
/// The fixture's "Mods" role (ADMINISTRATOR).
const MODS: u64 = 41_771_983_444_444_444;

fn id(value: u64) -> Snowflake {
    Snowflake(value)
}

fn apply(store: &mut Store, name: &str, raw: Value) {
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: decode_fixture(name, &raw.to_string()),
    });
    assert_eq!(store.bytes(), store.recount());
}

fn events() -> Vec<ScheduledEvent> {
    let list: Value = serde_json::from_str(LIST).unwrap();
    parse_list(list["list"].to_string().as_bytes()).unwrap()
}

/// The fixture guild holding the fixture events; the account gets `grant`
/// permission bits through @everyone, or the administrator role.
fn store(grant: u64, admin: bool) -> Store {
    let mut store = Store::new();
    let Dispatch::Ready(ready) = decode_fixture("READY", READY) else {
        panic!("READY did not decode");
    };
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: Dispatch::Ready(ready),
    });
    let roles: Vec<String> = admin.then(|| MODS.to_string()).into_iter().collect();
    apply(
        &mut store,
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": GUILD.to_string(), "user": {"id": ME.to_string()}, "roles": roles}),
    );
    apply(
        &mut store,
        "GUILD_ROLE_UPDATE",
        json!({"guild_id": GUILD.to_string(),
               "role": {"id": GUILD.to_string(), "name": "e", "permissions": grant.to_string()}}),
    );
    let loaded = store.replace_scheduled_events(id(GUILD), events());
    assert!(loaded.guilds.contains(&id(GUILD)));
    assert_eq!(store.bytes(), store.recount());
    store
}

fn draft() -> EventDraft {
    EventDraft {
        name: " Game night ".into(),
        description: String::new(),
        kind: EventKind::Voice,
        channel: Some(id(CHANNEL)),
        location: String::new(),
        start_ms: 2_000_000_000_000,
        end_ms: None,
    }
}

fn path(action: &EventAction) -> (Method, String, Option<Value>) {
    parts(id(GUILD), action)
}

#[test]
fn the_list_decodes_and_skips_events_it_does_not_know() {
    let events = events();
    assert_eq!(events.len(), 2, "the unknown kind is skipped");
    assert_eq!(events[0].user_count, Some(12));
    assert_eq!(events[1].kind, EventKind::External);
    assert_eq!(events[1].recurrence.as_deref(), Some("Every week on Mon"));
    assert!(parse_list(b"{}").is_none());
    assert!(parse_list(&vec![b' '; MAX_BODY_BYTES + 1]).is_none());
}

#[test]
fn routes_and_bodies() {
    let base = "/guilds/41771983423143937/scheduled-events";
    let (method, url, body) = path(&EventAction::Interested(id(900)));
    assert_eq!(
        (method, url, body),
        (Method::PUT, format!("{base}/900/users/@me"), None)
    );
    let (method, _, body) = path(&EventAction::NotInterested(id(900)));
    assert_eq!((method, body), (Method::DELETE, None));

    let (method, url, body) = path(&EventAction::Create(draft()));
    assert_eq!((method, url.as_str()), (Method::POST, base));
    assert_eq!(
        body.unwrap(),
        json!({"name": "Game night", "scheduled_start_time": "2033-05-18T03:33:20.000Z",
               "entity_type": 2, "privacy_level": 2, "channel_id": CHANNEL.to_string()})
    );

    // An edit clears what is empty and never mixes a channel with a location.
    let external = EventDraft {
        kind: EventKind::External,
        location: "Town hall".into(),
        end_ms: Some(2_000_000_600_000),
        ..draft()
    };
    let (method, url, body) = path(&EventAction::Edit(id(901), external));
    assert_eq!((method, url), (Method::PATCH, format!("{base}/901")));
    let body = body.unwrap();
    assert_eq!(body["entity_metadata"], json!({"location": "Town hall"}));
    assert_eq!(body["channel_id"], Value::Null);
    assert_eq!(body["description"], Value::Null);
    assert_eq!(body["scheduled_end_time"], "2033-05-18T03:43:20.000Z");
    let (_, _, body) = path(&EventAction::Edit(id(900), draft()));
    let body = body.unwrap();
    assert_eq!(body["entity_metadata"], Value::Null);
    assert_eq!(body["scheduled_end_time"], Value::Null);

    for (action, status) in [
        (EventAction::Start(id(900)), 2),
        (EventAction::End(id(900)), 3),
        (EventAction::Cancel(id(900)), 4),
    ] {
        let (method, _, body) = path(&action);
        assert_eq!(
            (method, body.unwrap()),
            (Method::PATCH, json!({"status": status}))
        );
    }
    let (method, _, body) = path(&EventAction::Delete(id(900)));
    assert_eq!((method, body), (Method::DELETE, None));
    assert!(request(id(GUILD), &EventAction::Create(draft())).is_ok());
}

#[test]
fn drafts_are_checked_before_they_are_sent() {
    let now = 1_000_000_000_000;
    assert_eq!(draft().check(now, true), Ok(()));
    let mut bad = draft();
    bad.name = "  ".into();
    assert_eq!(bad.check(now, true), Err(DraftError::Name));
    bad = draft();
    bad.description = "d".repeat(MAX_DESCRIPTION_CHARS + 1);
    assert_eq!(bad.check(now, true), Err(DraftError::Description));
    bad = draft();
    bad.channel = None;
    assert_eq!(bad.check(now, true), Err(DraftError::Channel));
    bad = draft();
    bad.start_ms = now;
    assert_eq!(bad.check(now, true), Err(DraftError::Start));
    assert_eq!(
        bad.check(now, false),
        Ok(()),
        "an edit that keeps the start"
    );
    bad = draft();
    bad.end_ms = Some(bad.start_ms);
    assert_eq!(bad.check(now, true), Err(DraftError::End));
    let external = EventDraft {
        kind: EventKind::External,
        channel: None,
        location: "Hall".into(),
        ..draft()
    };
    assert_eq!(external.check(now, true), Err(DraftError::EndRequired));
    let external = EventDraft {
        end_ms: Some(external.start_ms + 1),
        location: " ".into(),
        ..external
    };
    assert_eq!(external.check(now, true), Err(DraftError::Location));
}

#[test]
fn permissions_gate_every_action() {
    let create = 1u64 << 44;
    let manage = 1u64 << 33;
    // A plain member may only show interest.
    let plain = store(0, false);
    assert_eq!(
        plain.event_permissions(id(GUILD)),
        EventPermissions::default()
    );
    assert!(plain.event_allows(id(GUILD), &EventAction::Interested(id(900))));
    assert!(!plain.event_allows(id(GUILD), &EventAction::Create(draft())));
    assert!(!plain.event_allows(id(GUILD), &EventAction::Delete(id(900))));

    // CREATE_EVENTS creates, and manages only events it created (none here).
    let creator = store(create, false);
    assert!(creator.event_allows(id(GUILD), &EventAction::Create(draft())));
    assert!(!creator.event_allows(id(GUILD), &EventAction::Start(id(900))));

    // MANAGE_EVENTS manages all, but only along the lifecycle.
    let manager = store(manage, false);
    assert!(manager.event_allows(id(GUILD), &EventAction::Start(id(900))));
    assert!(
        !manager.event_allows(id(GUILD), &EventAction::End(id(900))),
        "not active"
    );
    assert!(manager.event_allows(id(GUILD), &EventAction::Cancel(id(900))));
    assert!(manager.event_allows(id(GUILD), &EventAction::End(id(901))));
    assert!(
        !manager.event_allows(id(GUILD), &EventAction::Cancel(id(901))),
        "active"
    );
    assert!(manager.event_allows(id(GUILD), &EventAction::Edit(id(900), draft())));
    assert!(manager.event_allows(id(GUILD), &EventAction::Delete(id(901))));
    // A channel the guild does not have, and an unknown event, send nothing.
    let stray = EventDraft {
        channel: Some(id(5)),
        ..draft()
    };
    assert!(!manager.event_allows(id(GUILD), &EventAction::Create(stray)));
    assert!(!manager.event_allows(id(GUILD), &EventAction::Delete(id(5))));
    assert!(!manager.event_allows(id(7), &EventAction::Create(draft())));

    // The administrator role stands for both bits.
    let admin = store(0, true);
    assert!(admin.event_permissions(id(GUILD)).manage);
}

#[test]
fn an_event_the_creator_owns_can_be_managed_with_create_events_alone() {
    let mut store = store(1 << 44, false);
    apply(
        &mut store,
        "GUILD_SCHEDULED_EVENT_CREATE",
        json!({"id": "910", "guild_id": GUILD.to_string(), "creator_id": ME.to_string(),
               "name": "Mine", "scheduled_start_time": "2026-12-01T10:00:00+00:00",
               "status": 1, "entity_type": 2, "channel_id": CHANNEL.to_string()}),
    );
    assert!(store.event_allows(id(GUILD), &EventAction::Start(id(910))));
    assert!(store.event_allows(id(GUILD), &EventAction::Delete(id(910))));
    assert!(!store.event_allows(id(GUILD), &EventAction::Start(id(900))));
}
