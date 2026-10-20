//! Message requests, Ignore and privacy settings in the reducer (issue #80).
//! Synthetic payloads in the observed web-client shape; unverified against a
//! live account (`docs/TESTING.md`).

use serde_json::json;

use super::navigation::Navigation;
use super::{Changes, Store, Suppression};
use crate::gateway::{GatewayEvent, decode_fixture};
use fastcord_model::{RelationshipKind, Snowflake};

fn id(n: u64) -> Snowflake {
    Snowflake(n)
}

fn apply(store: &mut Store, name: &str, raw: &str) -> Changes {
    let changes = store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: decode_fixture(name, raw),
    });
    assert_eq!(store.bytes(), store.recount(), "ledger drifted");
    changes
}

fn ready(relationships: serde_json::Value, channels: serde_json::Value) -> String {
    json!({
        "session_id": "s", "resume_gateway_url": "wss://gateway.discord.gg",
        "user": {"id": "1", "username": "me"}, "guilds": [],
        "users": [
            {"id": "20", "username": "stranger"},
            {"id": "21", "username": "friend"},
            {"id": "22", "username": "pal"}
        ],
        "relationships": relationships, "private_channels": channels,
    })
    .to_string()
}

fn user(n: u64) -> serde_json::Value {
    json!({"id": n.to_string(), "username": format!("u{n}")})
}

#[test]
fn ignore_is_its_own_flag_blocking_wins_and_the_account_is_never_suppressed() {
    let mut store = Store::new();
    let raw = ready(
        json!([
            {"id": "21", "type": 1, "user": user(21), "user_ignored": true},
            {"id": "22", "type": 2, "user": user(22), "user_ignored": true},
            {"id": "30", "type": 0, "user_ignored": true},
            {"id": "31", "type": 1, "user": user(31), "user_ignored": false},
            {"id": "1", "type": 1, "user": user(1), "user_ignored": true},
        ]),
        json!([]),
    );
    apply(&mut store, "READY", &raw);
    // Ignored, with its friendship intact.
    assert_eq!(store.suppression(id(21)), Some(Suppression::Ignored));
    assert_eq!(store.relationship(id(21)), Some(RelationshipKind::Friend));
    // Blocking wins over ignoring.
    assert_eq!(store.suppression(id(22)), Some(Suppression::Blocked));
    // An ignore with no kept relationship type still hides.
    assert_eq!(store.suppression(id(30)), Some(Suppression::Ignored));
    assert_eq!(store.relationship(id(30)), None);
    assert_eq!(store.suppression(id(31)), None);
    assert_eq!(store.suppression(id(1)), None, "never the account itself");
    assert_eq!(store.suppressed().len(), 4);
}

#[test]
fn events_converge_replays_and_omissions_change_nothing() {
    let mut store = Store::new();
    apply(&mut store, "READY", &ready(json!([]), json!([])));
    assert!(store.suppressed().is_empty());
    // An ignore for a user the account has no relationship with.
    let ignore = r#"{"id":"40","type":0,"user_ignored":true}"#;
    assert!(apply(&mut store, "RELATIONSHIP_ADD", ignore).relationships);
    assert_eq!(store.suppression(id(40)), Some(Suppression::Ignored));
    assert!(
        !apply(&mut store, "RELATIONSHIP_ADD", ignore).relationships,
        "replay"
    );
    // An update that omits the flag leaves it as it is.
    let nickname = r#"{"id":"40","nickname":"n"}"#;
    assert!(!apply(&mut store, "RELATIONSHIP_UPDATE", nickname).relationships);
    assert_eq!(store.suppression(id(40)), Some(Suppression::Ignored));
    // Another client stops ignoring.
    let off = r#"{"id":"40","user_ignored":false}"#;
    assert!(apply(&mut store, "RELATIONSHIP_UPDATE", off).relationships);
    assert_eq!(store.suppression(id(40)), None);
    assert!(
        !apply(&mut store, "RELATIONSHIP_UPDATE", off).relationships,
        "replay"
    );

    // A friend who is ignored stays ignored when unfriended (the flag is not
    // part of the friendship), and an add that omits the flag never clears it.
    let friend = json!({"id": "21", "type": 1, "user": user(21), "user_ignored": true}).to_string();
    apply(&mut store, "RELATIONSHIP_ADD", &friend);
    assert_eq!(store.suppression(id(21)), Some(Suppression::Ignored));
    apply(&mut store, "RELATIONSHIP_REMOVE", r#"{"id":"21","type":1}"#);
    assert_eq!(store.relationship(id(21)), None);
    assert_eq!(store.suppression(id(21)), Some(Suppression::Ignored));
    let readd = json!({"id": "21", "type": 1, "user": user(21)}).to_string();
    apply(&mut store, "RELATIONSHIP_ADD", &readd);
    assert_eq!(store.suppression(id(21)), Some(Suppression::Ignored));
    apply(
        &mut store,
        "RELATIONSHIP_UPDATE",
        r#"{"id":"21","user_ignored":false}"#,
    );
    assert_eq!(store.suppression(id(21)), None);

    // An ignore that existed only as an entry leaves with its removal.
    apply(&mut store, "RELATIONSHIP_ADD", ignore);
    assert!(apply(&mut store, "RELATIONSHIP_REMOVE", r#"{"id":"40"}"#).relationships);
    assert_eq!(store.suppression(id(40)), None);

    // A new session starts from what READY says.
    apply(&mut store, "RELATIONSHIP_ADD", ignore);
    apply(&mut store, "READY", &ready(json!([]), json!([])));
    assert!(store.suppressed().is_empty());
}

#[test]
fn blocking_later_overrides_and_unblocking_returns_to_ignored() {
    let mut store = Store::new();
    let raw = ready(
        json!([{"id": "21", "type": 1, "user": user(21), "user_ignored": true}]),
        json!([]),
    );
    apply(&mut store, "READY", &raw);
    apply(&mut store, "RELATIONSHIP_UPDATE", r#"{"id":"21","type":2}"#);
    assert_eq!(store.suppression(id(21)), Some(Suppression::Blocked));
    apply(&mut store, "RELATIONSHIP_UPDATE", r#"{"id":"21","type":1}"#);
    assert_eq!(store.suppression(id(21)), Some(Suppression::Ignored));
}

#[test]
fn the_ignored_set_is_bounded() {
    let mut store = Store::new();
    apply(&mut store, "READY", &ready(json!([]), json!([])));
    for n in 0..(crate::gateway::MAX_RELATIONSHIPS as u64 + 20) {
        let add = json!({"id": (1000 + n).to_string(), "type": 0, "user_ignored": true});
        apply(&mut store, "RELATIONSHIP_ADD", &add.to_string());
    }
    assert_eq!(store.suppressed().len(), crate::gateway::MAX_RELATIONSHIPS);
}

fn requests_ready() -> String {
    ready(
        json!([]),
        json!([
            {"id": "300", "type": 1, "recipient_ids": ["20"], "last_message_id": "900",
             "is_message_request": true, "is_spam": true},
            {"id": "301", "type": 1, "recipient_ids": ["21"], "last_message_id": "800",
             "is_message_request": true},
            {"id": "302", "type": 1, "recipient_ids": ["22"], "last_message_id": "700",
             "is_message_request": false},
            {"id": "303", "type": 3, "recipient_ids": ["20", "21"]}
        ]),
    )
}

#[test]
fn requests_are_listed_apart_never_selectable_and_never_unread_inbox_rows() {
    let mut store = Store::new();
    apply(&mut store, "READY", &requests_ready());
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let snapshot = navigation.snapshot_with_private(&store, 0, usize::MAX, 0, usize::MAX, 0..100);
    // Ordinary conversations exclude every pending request.
    let dms: Vec<_> = snapshot
        .private_channels
        .rows
        .iter()
        .map(|row| row.id)
        .collect();
    assert_eq!(dms.len(), 2);
    assert!(dms.contains(&id(302)) && dms.contains(&id(303)));
    // The requests are their own bounded list, newest first, spam marked.
    let requests = &snapshot.requests;
    assert_eq!(requests.total, 2);
    assert_eq!(
        requests
            .rows
            .iter()
            .map(|r| (r.channel, r.spam))
            .collect::<Vec<_>>(),
        [(id(300), true), (id(301), false)]
    );
    // Opening one as a conversation is refused, so nothing can read or
    // acknowledge it through the ordinary path.
    assert!(!navigation.select_private_channel(&store, id(300)));
    assert!(!navigation.open_channel(&store, id(300)));
    assert_eq!(navigation.inbox(&store, 64).rows.len(), 0);
}

#[test]
fn a_request_stays_pending_until_discord_says_otherwise() {
    let mut store = Store::new();
    apply(&mut store, "READY", &requests_ready());
    let mut navigation = Navigation::default();
    let pending = |store: &Store, navigation: &mut Navigation| {
        navigation.reconcile(store);
        navigation
            .snapshot(store, 0, usize::MAX, 0, usize::MAX)
            .requests
            .total
    };
    // A partial update without the flags does not accept the request.
    let rename = r#"{"id":"300","type":1,"last_message_id":"950"}"#;
    assert!(apply(&mut store, "CHANNEL_UPDATE", rename).private_channels);
    assert_eq!(pending(&store, &mut navigation), 2);
    // A replayed explicit "still a request" changes nothing visible.
    let still = r#"{"id":"300","type":1,"is_message_request":true,"is_spam":true}"#;
    apply(&mut store, "CHANNEL_UPDATE", still);
    assert_eq!(pending(&store, &mut navigation), 2);
    // Accepted (here or in another client): it joins the ordinary list.
    let accepted = r#"{"id":"300","type":1,"is_message_request":false}"#;
    apply(&mut store, "CHANNEL_UPDATE", accepted);
    assert_eq!(pending(&store, &mut navigation), 1);
    assert!(navigation.select_private_channel(&store, id(300)));
    // Declined: the channel is gone.
    apply(&mut store, "CHANNEL_DELETE", r#"{"id":"301","type":1}"#);
    assert_eq!(pending(&store, &mut navigation), 0);
}

#[test]
fn requests_are_capped_with_the_full_count_kept() {
    let mut store = Store::new();
    let channels: Vec<_> = (0..130u64)
        .map(|n| {
            json!({"id": (500 + n).to_string(), "type": 1, "recipient_ids": ["20"],
                         "is_message_request": true})
        })
        .collect();
    apply(&mut store, "READY", &ready(json!([]), json!(channels)));
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let requests = navigation
        .snapshot(&store, 0, usize::MAX, 0, usize::MAX)
        .requests;
    assert_eq!(requests.total, 130);
    assert_eq!(requests.rows.len(), super::navigation::MAX_REQUEST_ROWS);
}

#[test]
fn calls_from_suppressed_users_and_requests_do_not_ring() {
    let mut store = Store::new();
    let raw = ready(
        json!([
            {"id": "21", "type": 1, "user": user(21), "user_ignored": true},
            {"id": "22", "type": 1, "user": user(22)}
        ]),
        json!([
            {"id": "400", "type": 1, "recipient_ids": ["21"]},
            {"id": "401", "type": 1, "recipient_ids": ["22"]},
            {"id": "402", "type": 1, "recipient_ids": ["20"], "is_message_request": true},
            {"id": "403", "type": 3, "recipient_ids": ["21", "22"]}
        ]),
    );
    apply(&mut store, "READY", &raw);
    for channel in ["400", "401", "402", "403"] {
        let ring = format!(r#"{{"channel_id":"{channel}","ringing":["1"]}}"#);
        apply(&mut store, "CALL_CREATE", &ring);
    }
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let snapshot = navigation.snapshot(&store, 0, usize::MAX, 0, usize::MAX);
    let mut ringing: Vec<_> = snapshot
        .incoming_calls
        .iter()
        .map(|c| c.channel_id)
        .collect();
    ringing.sort();
    // The friend and the group ring; the ignored user and the request do not.
    assert_eq!(ringing, [id(401), id(403)]);
    // The reducer still knows the calls: the policy is presentation only.
    assert_eq!(store.calls().len(), 4);
}

#[test]
fn privacy_updates_from_other_clients_stamp_real_changes_only() {
    let mut store = Store::new();
    apply(&mut store, "READY", &ready(json!([]), json!([])));
    assert!(store.privacy().settings.is_empty());
    let flip = |restricted: bool| {
        json!({"default_guilds_restricted": restricted, "theme": "dark"}).to_string()
    };
    assert!(apply(&mut store, "USER_SETTINGS_UPDATE", &flip(true)).privacy);
    assert!(
        !apply(&mut store, "USER_SETTINGS_UPDATE", &flip(true)).privacy,
        "replay"
    );
    // Unrelated and odd updates change nothing.
    assert!(!apply(&mut store, "USER_SETTINGS_UPDATE", r#"{"theme":"light"}"#).privacy);
    assert!(
        !apply(
            &mut store,
            "USER_SETTINGS_UPDATE",
            r#"{"default_guilds_restricted":"yes","friend_source_flags":{"all":1}}"#
        )
        .privacy
    );
    let live = store.privacy();
    assert_eq!(live.settings.default_guilds_restricted, Some(true));
    assert_eq!(live.stamps[1], live.latest);
    let before = live.latest;
    // A new session forgets the overlay but never reuses a stamp.
    apply(&mut store, "READY", &ready(json!([]), json!([])));
    assert!(store.privacy().settings.is_empty());
    assert!(apply(&mut store, "USER_SETTINGS_UPDATE", &flip(false)).privacy);
    assert!(store.privacy().latest > before);
}
