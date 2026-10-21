use super::*;
use crate::gateway::{Dispatch, GatewayEvent, decode_fixture};
use crate::state::Store;
use crate::state::navigation::{ChannelKind as RowKind, Navigation, Occupant, StageSeat};
use fastcord_model::{Channel, ChannelKind};
use serde_json::json;

const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
const ME: u64 = 175_928_847_299_117_063;
const GUILD: u64 = 41_771_983_423_143_937;
const CHANNEL: u64 = 41_771_983_423_143_939;
/// The fixture's "Mods" role (ADMINISTRATOR).
const MODS: u64 = 41_771_983_444_444_444;

fn id(value: u64) -> Snowflake {
    Snowflake(value)
}

fn apply(store: &mut Store, name: &str, raw: serde_json::Value) -> crate::state::Changes {
    let event = GatewayEvent::Dispatch {
        sequence: 1,
        event: decode_fixture(name, &raw.to_string()),
    };
    let changes = store.apply(event);
    assert_eq!(store.bytes(), store.recount());
    changes
}

/// The fixture guild with its voice channel turned into a Stage, the account a
/// plain member (or, with `moderator`, holding the administrator role).
fn store(moderator: bool) -> Store {
    let mut store = Store::new();
    let Dispatch::Ready(ready) = decode_fixture("READY", READY) else {
        panic!("READY did not decode");
    };
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: Dispatch::Ready(ready),
    });
    let mut stage: Channel = store
        .guild(id(GUILD))
        .and_then(|g| g.channel(id(CHANNEL)))
        .cloned()
        .unwrap();
    stage.kind = ChannelKind::GuildStageVoice;
    stage.permission_overwrites.clear();
    store.apply(GatewayEvent::Dispatch {
        sequence: 2,
        event: Dispatch::ChannelUpdate(Box::new(stage)),
    });
    let roles: Vec<String> = moderator.then(|| MODS.to_string()).into_iter().collect();
    apply(
        &mut store,
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": GUILD.to_string(), "user": {"id": ME.to_string()}, "roles": roles}),
    );
    store
}

fn voice(store: &mut Store, user: u64, suppress: bool, hand: Option<&str>) {
    apply(
        store,
        "VOICE_STATE_UPDATE",
        json!({
            "guild_id": GUILD.to_string(), "channel_id": CHANNEL.to_string(),
            "user_id": user.to_string(), "session_id": "s",
            "suppress": suppress, "request_to_speak_timestamp": hand,
        }),
    );
}

fn instance(store: &mut Store, name: &str, topic: &str) -> crate::state::Changes {
    apply(
        store,
        name,
        json!({
            "id": "9", "guild_id": GUILD.to_string(), "channel_id": CHANNEL.to_string(),
            "topic": topic, "privacy_level": 2,
        }),
    )
}

#[test]
fn a_voice_state_carries_suppression_and_a_raised_hand() {
    let Dispatch::VoiceStateUpdate(update) = decode_fixture(
        "VOICE_STATE_UPDATE",
        r#"{"guild_id":"1","channel_id":"2","user_id":"3","session_id":"s","suppress":true,
            "request_to_speak_timestamp":"2026-10-21T10:00:00.000000+00:00"}"#,
    ) else {
        panic!("VOICE_STATE_UPDATE did not decode");
    };
    assert!(update.state.suppress);
    assert_eq!(
        update.state.request_to_speak_timestamp.as_deref(),
        Some("2026-10-21T10:00:00.000000+00:00")
    );
    let Dispatch::VoiceStateUpdate(plain) = decode_fixture(
        "VOICE_STATE_UPDATE",
        r#"{"guild_id":"1","channel_id":"2","user_id":"3","request_to_speak_timestamp":null}"#,
    ) else {
        panic!("VOICE_STATE_UPDATE did not decode");
    };
    assert!(!plain.state.suppress && plain.state.request_to_speak_timestamp.is_none());
}

#[test]
fn stage_instances_start_change_end_and_replays_settle() {
    let mut store = store(false);
    let topic = |store: &Store| {
        store
            .guild(id(GUILD))
            .and_then(|g| g.stage_instance(id(CHANNEL)))
            .map(|s| s.topic.clone())
    };
    assert_eq!(topic(&store), None);
    assert!(!instance(&mut store, "STAGE_INSTANCE_CREATE", "Q and A").is_empty());
    assert_eq!(topic(&store).as_deref(), Some("Q and A"));
    // A replay changes nothing; an update replaces the topic in place.
    assert!(instance(&mut store, "STAGE_INSTANCE_CREATE", "Q and A").is_empty());
    assert!(!instance(&mut store, "STAGE_INSTANCE_UPDATE", "Roadmap").is_empty());
    assert_eq!(topic(&store).as_deref(), Some("Roadmap"));
    // A topic longer than Discord allows is cut, never stored whole.
    instance(&mut store, "STAGE_INSTANCE_UPDATE", &"x".repeat(500));
    assert_eq!(topic(&store).unwrap().chars().count(), MAX_TOPIC_CHARS);
    assert!(!instance(&mut store, "STAGE_INSTANCE_DELETE", "").is_empty());
    assert_eq!(topic(&store), None);
    // A stale delete of a session already gone is ignored.
    assert!(instance(&mut store, "STAGE_INSTANCE_DELETE", "").is_empty());
}

#[test]
fn every_action_sends_one_minimal_body_to_its_route() {
    let (g, c, u) = (id(1), id(2), id(3));
    let now = "2026-10-21T10:00:00.000Z";
    let of = |action: StageAction| parts(g, c, &action, now);
    let (method, path, body) = of(StageAction::RequestToSpeak);
    assert_eq!(
        (method, path.as_str()),
        (Method::PATCH, "/guilds/1/voice-states/@me")
    );
    assert_eq!(
        body,
        Some(json!({"channel_id": "2", "request_to_speak_timestamp": now}))
    );
    assert_eq!(
        of(StageAction::CancelRequest).2,
        Some(json!({"channel_id": "2", "request_to_speak_timestamp": null}))
    );
    assert_eq!(
        of(StageAction::BecomeSpeaker).2,
        Some(json!({"channel_id": "2", "suppress": false}))
    );
    assert_eq!(
        of(StageAction::MoveToAudience).2,
        Some(json!({"channel_id": "2", "suppress": true}))
    );
    let (method, path, body) = of(StageAction::Invite(u));
    assert_eq!(
        (method, path.as_str()),
        (Method::PATCH, "/guilds/1/voice-states/3")
    );
    assert_eq!(body, Some(json!({"channel_id": "2", "suppress": false})));
    assert_eq!(
        of(StageAction::Demote(u)).2,
        Some(json!({"channel_id": "2", "suppress": true}))
    );
    let (method, path, body) = of(StageAction::Start("  Hello  ".into()));
    assert_eq!((method, path.as_str()), (Method::POST, "/stage-instances"));
    assert_eq!(
        body,
        Some(json!({"channel_id": "2", "topic": "Hello", "privacy_level": 2}))
    );
    let (method, path, body) = of(StageAction::SetTopic("New".into()));
    assert_eq!(
        (method, path.as_str()),
        (Method::PATCH, "/stage-instances/2")
    );
    assert_eq!(body, Some(json!({"topic": "New"})));
    let (method, path, body) = of(StageAction::End);
    assert_eq!(
        (method, path.as_str()),
        (Method::DELETE, "/stage-instances/2")
    );
    assert_eq!(body, None);
    for action in [
        StageAction::RequestToSpeak,
        StageAction::Invite(u),
        StageAction::End,
    ] {
        assert!(request(g, c, &action, now).is_ok());
    }
}

#[test]
fn time_is_formatted_as_utc_iso_8601() {
    assert_eq!(iso8601(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso8601(951_782_400), "2000-02-29T00:00:00.000Z");
    assert_eq!(iso8601(1_792_576_496), "2026-10-21T09:54:56.000Z");
}

#[test]
fn topics_are_trimmed_and_bounded() {
    assert_eq!(topic("  hi "), Some("hi"));
    assert_eq!(topic("   "), None);
    assert_eq!(topic(&"x".repeat(120)).map(str::len), Some(120));
    assert_eq!(topic(&"x".repeat(121)), None);
}

#[test]
fn an_ordinary_member_listens_and_may_only_raise_a_hand() {
    let mut store = store(false);
    let permissions = store.stage_permissions(id(GUILD), id(CHANNEL));
    assert!(permissions.request && !permissions.moderate && !permissions.manage);
    let allows =
        |store: &Store, action: StageAction| store.stage_allows(id(GUILD), id(CHANNEL), &action);
    // Not in the Stage yet: nothing about the own voice state is offered.
    assert!(!allows(&store, StageAction::RequestToSpeak));
    voice(&mut store, ME, true, None);
    voice(&mut store, 7, true, None);
    voice(&mut store, 8, false, None);
    assert!(allows(&store, StageAction::RequestToSpeak));
    assert!(!allows(&store, StageAction::CancelRequest));
    assert!(!allows(&store, StageAction::BecomeSpeaker), "no invitation");
    assert!(
        !allows(&store, StageAction::MoveToAudience),
        "already audience"
    );
    // Moderation and the session are for moderators only.
    assert!(!allows(&store, StageAction::Invite(id(7))));
    assert!(!allows(&store, StageAction::Demote(id(8))));
    assert!(!allows(&store, StageAction::Start("Topic".into())));
    voice(
        &mut store,
        ME,
        true,
        Some("2026-10-21T10:00:00.000000+00:00"),
    );
    assert!(
        !allows(&store, StageAction::RequestToSpeak),
        "one request at a time"
    );
    assert!(allows(&store, StageAction::CancelRequest));
    assert!(
        allows(&store, StageAction::BecomeSpeaker),
        "an approved request can be taken"
    );
    voice(&mut store, ME, false, None);
    assert!(allows(&store, StageAction::MoveToAudience));
    assert!(!allows(&store, StageAction::BecomeSpeaker));
}

#[test]
fn a_moderator_invites_demotes_and_runs_the_session() {
    let mut store = store(true);
    let permissions = store.stage_permissions(id(GUILD), id(CHANNEL));
    assert!(permissions.moderate && permissions.manage);
    let allows =
        |store: &Store, action: StageAction| store.stage_allows(id(GUILD), id(CHANNEL), &action);
    voice(&mut store, ME, true, None);
    voice(
        &mut store,
        7,
        true,
        Some("2026-10-21T10:00:00.000000+00:00"),
    );
    voice(&mut store, 8, false, None);
    assert!(
        allows(&store, StageAction::Invite(id(7))),
        "approve a request"
    );
    assert!(
        !allows(&store, StageAction::Invite(id(8))),
        "already a speaker"
    );
    assert!(allows(&store, StageAction::Demote(id(8))));
    assert!(
        !allows(&store, StageAction::Demote(id(7))),
        "already audience"
    );
    assert!(
        !allows(&store, StageAction::Invite(id(ME))),
        "never through the menu on oneself"
    );
    assert!(
        !allows(&store, StageAction::Invite(id(99))),
        "not in the Stage"
    );
    assert!(
        allows(&store, StageAction::BecomeSpeaker),
        "a moderator speaks directly"
    );
    // Start only without a session; edit and end only with one.
    assert!(allows(&store, StageAction::Start("Topic".into())));
    assert!(
        !allows(&store, StageAction::Start("   ".into())),
        "a topic is required"
    );
    assert!(!allows(&store, StageAction::SetTopic("New".into())));
    assert!(!allows(&store, StageAction::End));
    instance(&mut store, "STAGE_INSTANCE_CREATE", "Topic");
    assert!(!allows(&store, StageAction::Start("Topic".into())));
    assert!(allows(&store, StageAction::SetTopic("New".into())));
    assert!(!allows(&store, StageAction::SetTopic("x".repeat(121))));
    assert!(allows(&store, StageAction::End));
}

#[test]
fn a_voice_channel_is_not_a_stage() {
    let mut store = Store::new();
    let Dispatch::Ready(ready) = decode_fixture("READY", READY) else {
        panic!("READY did not decode");
    };
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: Dispatch::Ready(ready),
    });
    assert_eq!(
        store.stage_permissions(id(GUILD), id(CHANNEL)),
        StagePermissions::default()
    );
    assert!(!store.stage_allows(id(GUILD), id(CHANNEL), &StageAction::End));
}

#[test]
fn a_stage_lists_speakers_then_the_audience_with_raised_hands_first() {
    let mut store = store(false);
    voice(&mut store, 5, true, None);
    voice(
        &mut store,
        6,
        true,
        Some("2026-10-21T10:05:00.000000+00:00"),
    );
    voice(
        &mut store,
        7,
        true,
        Some("2026-10-21T10:01:00.000000+00:00"),
    );
    voice(&mut store, 9, false, None);
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let rows = navigation.channel_window(&store, 0, usize::MAX).rows;
    let stage = rows
        .iter()
        .find(|row| row.id == id(CHANNEL) && row.occupant.is_none())
        .unwrap();
    assert!(stage.stage && stage.kind == RowKind::Voice);
    let lines: Vec<_> = rows.iter().filter_map(|row| row.occupant.clone()).collect();
    let shape: Vec<String> = lines
        .iter()
        .map(|line| match line {
            Occupant::Heading(text) => (*text).to_owned(),
            Occupant::Member { user_id, seat, .. } => format!("{}:{seat:?}", user_id.0),
            Occupant::More(n) => format!("+{n}"),
        })
        .collect();
    assert_eq!(
        shape,
        [
            "Speakers",
            "9:Some(Speaker)",
            "Audience",
            "7:Some(Hand)",
            "6:Some(Hand)",
            "5:Some(Audience)"
        ]
    );
    // The windowed total counts the heading lines too.
    let all = navigation.channel_window(&store, 0, usize::MAX);
    assert_eq!(all.total, all.rows.len());
    assert_eq!(
        StageSeat::of(
            &store
                .guild(id(GUILD))
                .unwrap()
                .voice_state(id(9))
                .cloned()
                .unwrap()
        ),
        StageSeat::Speaker
    );
}

#[test]
fn an_ordinary_voice_channel_has_no_headings_or_seats() {
    let mut store = Store::new();
    let Dispatch::Ready(ready) = decode_fixture("READY", READY) else {
        panic!("READY did not decode");
    };
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: Dispatch::Ready(ready),
    });
    voice(&mut store, 5, true, None);
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let rows = navigation.channel_window(&store, 0, usize::MAX).rows;
    assert!(rows.iter().all(|row| !row.stage));
    assert!(
        rows.iter()
            .filter_map(|row| row.occupant.as_ref())
            .all(|line| matches!(line, Occupant::Member { seat: None, .. }))
    );
}
