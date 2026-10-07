//! Reducer behavior against recorded payloads and generated large accounts.

use fastcord_model::{
    ChannelKind, OverwriteKind, PermissionOverwrite, Permissions, Role, VoiceState,
};

use super::*;
use crate::gateway::{
    ChannelUnread, GroupId, GuildDelete, ListRow, MemberListId, MemberListOp, MessageDelete,
    SessionId, decode_fixture,
};

const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
const SUPPLEMENTAL: &str = include_str!("../../../../fixtures/gateway/ready_supplemental.json");
const PASSIVE: &str = include_str!("../../../../fixtures/gateway/passive_update_v2.json");
const LIST_SYNC: &str = include_str!("../../../../fixtures/gateway/member_list_update_sync.json");
const LIST_OPS: &str = include_str!("../../../../fixtures/gateway/member_list_update_ops.json");
const VOICE_UPDATE: &str = include_str!("../../../../fixtures/gateway/voice_state_update.json");
const MEMBER_UPDATE: &str = include_str!("../../../../fixtures/gateway/guild_member_update.json");
const MESSAGE: &str = include_str!("../../../../fixtures/model/message_create.json");

const ME: u64 = 175_928_847_299_117_063;
const NELLY: u64 = 80_351_110_224_678_912;
const MASON: u64 = 53_908_232_506_183_680;
const NEWCOMER: u64 = 60_606_060_606_060_606;
/// "Test Server" and "Second Server" of the READY fixture.
const FIRST: u64 = 41_771_983_423_143_937;
const SECOND: u64 = 41_771_983_423_143_941;
const OUTAGE: u64 = 41_771_983_423_143_940;
const GENERAL: u64 = 41_771_983_423_143_938;
const VOICE: u64 = 41_771_983_423_143_939;
const LOBBY: u64 = 41_771_983_423_143_942;

fn id(value: u64) -> Snowflake {
    Snowflake(value)
}

fn event(dispatch: Dispatch) -> GatewayEvent {
    GatewayEvent::Dispatch {
        sequence: 1,
        event: dispatch,
    }
}

fn fixture(name: &str, raw: &str) -> GatewayEvent {
    event(decode_fixture(name, raw))
}

fn check(store: &Store) {
    assert_eq!(
        store.bytes(),
        store.recount(),
        "the incremental ledger drifted from the contents"
    );
}

fn apply(store: &mut Store, name: &str, raw: &str) -> Changes {
    let changes = store.apply(fixture(name, raw));
    check(store);
    changes
}

fn ready_store() -> Store {
    let mut store = Store::new();
    apply(&mut store, "READY", READY);
    store
}

fn select(guild: u64, channel: u64) -> SubscriptionTarget {
    let mut target = SubscriptionTarget::default();
    target.select_channel(id(guild), id(channel));
    target
}

fn sorted<const N: usize>(mut ids: [u64; N]) -> [u64; N] {
    ids.sort_unstable();
    ids
}

fn member_ids(store: &Store, guild: u64) -> Vec<u64> {
    let mut ids: Vec<u64> = store
        .guild(id(guild))
        .unwrap()
        .members
        .keys()
        .map(|user| user.0)
        .collect();
    ids.sort_unstable();
    ids
}

fn list_users(store: &Store, guild: u64) -> Vec<(u32, u64)> {
    let list = store.guild(id(guild)).unwrap().member_list().unwrap();
    list.rows(0, u32::MAX)
        .map(|(index, row)| match row {
            ListRow::Member(user) => (index, user.0),
            ListRow::Group(_) => (index, 0),
            ListRow::Unreadable => (index, u64::MAX),
        })
        .collect()
}

#[test]
fn ready_waits_for_supplemental_before_forgetting_users_nothing_references() {
    let Dispatch::Ready(mut ready) = decode_fixture("READY", READY) else {
        panic!("READY did not decode");
    };
    // A friend-list user that no guild, DM, or voice state references.
    ready.users.push(User {
        id: id(999),
        username: "stranger".to_owned(),
        global_name: None,
        avatar: None,
        bot: false,
    });
    let mut store = Store::new();
    let changes = store.apply(event(Dispatch::Ready(ready)));
    check(&store);
    assert!(changes.reset);

    assert_eq!(store.guild_ids(), [id(FIRST), id(SECOND)]);
    assert_eq!(store.unavailable_guilds().collect::<Vec<_>>(), [id(OUTAGE)]);
    let first = store.guild(id(FIRST)).unwrap();
    assert_eq!(first.name(), "Test Server");
    assert_eq!(first.channels().count(), 3);
    assert_eq!(
        first.channel(id(GENERAL)).unwrap().last_message_id,
        Some(id(175_928_847_299_117_100))
    );
    assert_eq!(first.roles().len(), 2);
    assert_eq!(first.member(id(ME)).unwrap().nick.as_deref(), Some("Altie"));
    assert_eq!(store.private_channels().count(), 2);

    assert_eq!(store.current_user().unwrap().username, "alt_fixture");
    assert!(
        store.user(id(999)).is_some(),
        "deduplicated users may acquire references in READY_SUPPLEMENTAL"
    );
    store.apply(event(Dispatch::ReadySupplemental(Box::new(
        ReadySupplemental {
            guilds: Vec::new(),
            users: Vec::new(),
            lazy_private_channels: Vec::new(),
        },
    ))));
    check(&store);
    assert!(
        store.user(id(999)).is_none(),
        "unreferenced users are not kept"
    );
    // The DM and group-DM participants and the current user.
    assert_eq!(store.users_held(), 4);
    assert!(store.user(id(MASON)).is_some());
}

#[test]
fn a_new_ready_replaces_the_session_but_not_what_is_on_screen() {
    let mut store = ready_store();
    store.focus(&select(FIRST, GENERAL));
    apply(&mut store, "READY_SUPPLEMENTAL", SUPPLEMENTAL);
    assert!(
        store
            .guild(id(FIRST))
            .unwrap()
            .voice_state(id(NELLY))
            .is_some()
    );
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_some());

    let changes = apply(&mut store, "READY", READY);
    assert!(changes.reset);
    let first = store.guild(id(FIRST)).unwrap();
    assert!(
        first.voice_state(id(NELLY)).is_none(),
        "voice is session state"
    );
    assert!(first.member_list().is_none(), "lists are re-synchronized");
    assert_eq!(first.members_held(), 1);
    // The consumer is still looking at the same channel, so its list is
    // accepted again once the subscription is re-sent and synchronized.
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_some());
}

#[test]
fn supplemental_fills_voice_and_members_by_guild() {
    let mut store = ready_store();
    let changes = apply(&mut store, "READY_SUPPLEMENTAL", SUPPLEMENTAL);
    assert_eq!(changes.voice, BTreeSet::from([id(FIRST), id(SECOND)]));
    let first = store.guild(id(FIRST)).unwrap();
    let voice = first.voice_state(id(NELLY)).unwrap();
    assert_eq!(voice.channel_id, Some(id(VOICE)));
    assert!(voice.self_mute);
    // Members aligned with their own guild; the outage guild's empty slot
    // never shifts the third guild's member onto the wrong one.
    assert_eq!(
        member_ids(&store, FIRST),
        sorted([ME, MASON, NELLY, NEWCOMER])
    );
    assert_eq!(member_ids(&store, SECOND), sorted([ME, NELLY]));
    assert_eq!(store.user(id(NEWCOMER)).unwrap().username, "newcomer");
    // Lazy DMs: one by ID, one with an embedded user that is stored once.
    assert_eq!(store.private_channels().count(), 4);
    let lazy = store.private_channel(id(300_000_000_000_000_004)).unwrap();
    assert!(lazy.recipients.is_empty());
    assert_eq!(lazy.recipient_ids, [id(70_707_070_707_070_707)]);
    assert!(store.user(id(70_707_070_707_070_707)).is_some());
}

#[test]
fn voice_membership_stays_fresh_in_other_guilds_while_browsing() {
    let mut store = ready_store();
    apply(&mut store, "READY_SUPPLEMENTAL", SUPPLEMENTAL);
    // Browsing the second guild while in a call in the first.
    let mut target = select(SECOND, LOBBY);
    target.set_voice_guild(Some(id(FIRST)));
    store.focus(&target);

    // The subscribed voice guild reports live voice state changes.
    let changes = apply(&mut store, "VOICE_STATE_UPDATE", VOICE_UPDATE);
    assert_eq!(changes.voice, BTreeSet::from([id(FIRST)]));
    let first = store.guild(id(FIRST)).unwrap();
    assert_eq!(
        first.voice_state(id(MASON)).unwrap().channel_id,
        Some(id(VOICE))
    );
    assert_eq!(first.member(id(MASON)).unwrap().nick.as_deref(), Some("M"));
    assert_eq!(first.voice_states().count(), 2);

    // Someone leaves.
    apply(
        &mut store,
        "VOICE_STATE_UPDATE",
        &format!(
            r#"{{"guild_id":"{FIRST}","channel_id":null,"user_id":"{NELLY}","session_id":"x"}}"#
        ),
    );
    let first = store.guild(id(FIRST)).unwrap();
    assert!(first.voice_state(id(NELLY)).is_none());
    assert_eq!(first.voice_states().count(), 1);

    // An unsubscribed third guild's voice and unread markers arrive passively.
    let changes = apply(&mut store, "PASSIVE_UPDATE_V2", PASSIVE);
    assert_eq!(changes.guilds, BTreeSet::from([id(SECOND)]));
    let second = store.guild(id(SECOND)).unwrap();
    assert_eq!(
        second.channel(id(LOBBY)).unwrap().last_message_id,
        Some(id(175_928_847_299_200_000))
    );
    assert_eq!(
        second.voice_state(id(NELLY)).unwrap().session_id,
        "voice-session-fixture"
    );
    assert_eq!(second.member(id(NELLY)).unwrap().nick.as_deref(), Some("N"));
    assert_eq!(store.user(id(NELLY)).unwrap().username, "nelly");
}

#[test]
fn passive_updates_never_regress_markers_and_ignore_unknown_guilds() {
    let mut store = ready_store();
    let newer = r#"{"guild_id":"41771983423143941","updated_channels":[{"id":"41771983423143942","last_message_id":"900"}]}"#;
    apply(&mut store, "PASSIVE_UPDATE_V2", PASSIVE);
    let older = newer.replace("900", "100");
    apply(&mut store, "PASSIVE_UPDATE_V2", &older);
    assert_eq!(
        store
            .guild(id(SECOND))
            .unwrap()
            .channel(id(LOBBY))
            .unwrap()
            .last_message_id,
        Some(id(175_928_847_299_200_000))
    );
    // A channel that is not (yet) known is not invented.
    let unknown_channel = r#"{"guild_id":"41771983423143941","updated_channels":[{"id":"7","last_message_id":"900"}]}"#;
    apply(&mut store, "PASSIVE_UPDATE_V2", unknown_channel);
    assert!(store.guild(id(SECOND)).unwrap().channel(id(7)).is_none());
    // A guild we do not hold gets nothing, not even its users.
    let before = store.users_held();
    let stranger = PASSIVE.replace("41771983423143941", "5");
    let changes = apply(&mut store, "PASSIVE_UPDATE_V2", &stranger);
    assert!(changes.is_empty());
    assert_eq!(store.users_held(), before);
    // A removed voice state of someone not in voice is harmless.
    apply(&mut store, "PASSIVE_UPDATE_V2", PASSIVE);
    apply(&mut store, "PASSIVE_UPDATE_V2", PASSIVE);
    assert_eq!(store.guild(id(SECOND)).unwrap().voice_states().count(), 1);
}

#[test]
fn the_visible_member_list_applies_sync_then_operations_index_correctly() {
    let mut store = ready_store();
    // Nothing is subscribed yet: a list nobody asked for is not kept.
    let changes = apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert!(changes.is_empty());
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_none());

    store.focus(&select(FIRST, GENERAL));
    let changes = apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert_eq!(changes.member_lists, BTreeSet::from([id(FIRST)]));
    let first = store.guild(id(FIRST)).unwrap();
    let list = first.member_list().unwrap();
    assert_eq!(list.id(), &MemberListId("everyone".to_owned()));
    assert_eq!((list.member_count(), list.online_count()), (4, 3));
    assert_eq!(first.member_count(), 4);
    assert_eq!(
        list_users(&store, FIRST),
        [
            (0, 0),
            (1, ME),
            (2, 0),
            (3, NELLY),
            (4, MASON),
            (5, 0),
            (6, NEWCOMER)
        ]
    );
    // Members and users arrive with their rows.
    assert_eq!(
        member_ids(&store, FIRST),
        sorted([ME, MASON, NELLY, NEWCOMER])
    );
    assert_eq!(store.user(id(NEWCOMER)).unwrap().username, "newcomer");

    // nelly goes offline, mason gets a nickname, the offline count grows.
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_OPS);
    assert_eq!(
        list_users(&store, FIRST),
        [
            (0, 0),
            (1, ME),
            (2, 0),
            (3, MASON),
            (4, 0),
            (5, NELLY),
            (6, NEWCOMER)
        ]
    );
    let list = store.guild(id(FIRST)).unwrap().member_list().unwrap();
    assert_eq!(list.online_count(), 2);
    let offline = list
        .groups()
        .iter()
        .find(|g| g.id == GroupId::Offline)
        .unwrap();
    assert_eq!(offline.count, 2);
    assert_eq!(
        store
            .guild(id(FIRST))
            .unwrap()
            .member(id(MASON))
            .unwrap()
            .nick
            .as_deref(),
        Some("M")
    );
}

#[test]
fn an_unknown_or_hostile_list_operation_cannot_corrupt_the_rest_of_the_store() {
    let mut store = ready_store();
    store.focus(&select(FIRST, GENERAL));
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    let hostile = format!(
        r#"{{"id":"everyone","guild_id":"{FIRST}","ops":[{{"op":"DELETE","index":4000000000}},{{"op":"INSERT","index":4294967295,"item":{{"group":{{"id":"online","count":1}}}}}},{{"op":"WHATEVER"}}]}}"#
    );
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &hostile);
    // The unknown operation emptied the list rather than leaving it wrong.
    let list = store.guild(id(FIRST)).unwrap().member_list().unwrap();
    assert_eq!(list.known_rows(), 0);
    assert_eq!(list.member_count(), 4);
    // The members themselves are still known.
    assert!(store.guild(id(FIRST)).unwrap().member(id(NELLY)).is_some());
}

#[test]
fn navigation_releases_member_lists_and_late_events_for_left_guilds_are_ignored() {
    let mut store = ready_store();
    store.focus(&select(FIRST, GENERAL));
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    let held = store.users_held();
    assert!(held >= 5);

    // Same guild, same channel: nothing is released.
    let changes = store.focus(&select(FIRST, GENERAL));
    assert!(changes.is_empty());
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_some());

    // Another channel releases the rows, even when its list key is shared.
    let changes = store.focus(&select(FIRST, VOICE));
    assert_eq!(changes.member_lists, BTreeSet::from([id(FIRST)]));
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_none());
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert_eq!(
        store.guild(id(FIRST)).unwrap().member_list().unwrap().id(),
        &MemberListId("everyone".to_owned())
    );

    // Navigating to another guild releases it, and the old guild's late
    // events no longer recreate anything.
    let changes = store.focus(&select(SECOND, LOBBY));
    assert_eq!(changes.member_lists, BTreeSet::from([id(FIRST)]));
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_none());
    let changes = apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert!(changes.is_empty());
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_none());

    // Clearing the selection releases everything.
    store.focus(&select(SECOND, LOBBY));
    let mut cleared = SubscriptionTarget::default();
    cleared.clear_selection();
    store.focus(&cleared);
    check(&store);
}

#[test]
fn arbitrary_list_ids_cannot_accumulate_in_a_guild() {
    let mut store = ready_store();
    store.focus(&select(FIRST, GENERAL));
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    for n in 0..6 {
        let raw = LIST_SYNC.replace(r#""id": "everyone""#, &format!(r#""id": "list-{n}""#));
        assert!(apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &raw).is_empty());
        assert_eq!(store.guild(id(FIRST)).unwrap().lists.len(), 1);
    }
    let visible = store.guild(id(FIRST)).unwrap().member_list().unwrap();
    assert_eq!(visible.id(), &MemberListId("everyone".to_owned()));
}

#[test]
fn member_events_keep_members_and_counts_current() {
    let mut store = ready_store();
    let before = store.guild(id(FIRST)).unwrap().member_count();
    let changes = apply(&mut store, "GUILD_MEMBER_ADD", MEMBER_UPDATE);
    assert_eq!(changes.guilds, BTreeSet::from([id(FIRST)]));
    assert_eq!(store.guild(id(FIRST)).unwrap().member_count(), before + 1);
    assert_eq!(
        store
            .guild(id(FIRST))
            .unwrap()
            .member(id(NELLY))
            .unwrap()
            .nick
            .as_deref(),
        Some("Nell")
    );
    // An update of a member we already hold changes the member, not the count.
    let renamed = MEMBER_UPDATE.replace("Nell", "Nelly!");
    apply(&mut store, "GUILD_MEMBER_UPDATE", &renamed);
    assert_eq!(store.guild(id(FIRST)).unwrap().member_count(), before + 1);
    assert_eq!(
        store
            .guild(id(FIRST))
            .unwrap()
            .member(id(NELLY))
            .unwrap()
            .nick
            .as_deref(),
        Some("Nelly!")
    );
    // A duplicate add (replayed) does not double count.
    apply(&mut store, "GUILD_MEMBER_ADD", MEMBER_UPDATE);
    assert_eq!(store.guild(id(FIRST)).unwrap().member_count(), before + 1);

    apply(
        &mut store,
        "GUILD_MEMBER_REMOVE",
        &format!(r#"{{"guild_id":"{FIRST}","user":{{"id":"{NELLY}","username":"nelly"}}}}"#),
    );
    assert_eq!(store.guild(id(FIRST)).unwrap().member_count(), before);
    assert!(store.guild(id(FIRST)).unwrap().member(id(NELLY)).is_none());
    // Events for guilds we do not hold change nothing.
    let stranger = MEMBER_UPDATE.replace(&FIRST.to_string(), "5");
    assert!(apply(&mut store, "GUILD_MEMBER_ADD", &stranger).is_empty());
}

#[test]
fn guild_and_channel_lifecycle_events_are_idempotent() {
    let mut store = ready_store();

    // GUILD_CREATE for a guild we hold refreshes its description and keeps
    // what was learned since, without regressing the unread marker.
    apply(&mut store, "READY_SUPPLEMENTAL", SUPPLEMENTAL);
    let refresh = format!(
        r#"{{"id":"{FIRST}","name":"Renamed","owner_id":"{NELLY}","member_count":43,"roles":[],"channels":[{{"id":"{GENERAL}","type":0,"name":"general","last_message_id":"5"}}],"members":[]}}"#
    );
    apply(&mut store, "GUILD_CREATE", &refresh);
    apply(&mut store, "GUILD_CREATE", &refresh);
    let first = store.guild(id(FIRST)).unwrap();
    assert_eq!(first.name(), "Renamed");
    assert_eq!(first.channels().count(), 1);
    assert_eq!(
        first.channel(id(GENERAL)).unwrap().last_message_id,
        Some(id(175_928_847_299_117_100))
    );
    assert_eq!(first.members_held(), 4);
    assert_eq!(first.voice_states().count(), 1);
    assert_eq!(
        store
            .guild_ids()
            .iter()
            .filter(|g| **g == id(FIRST))
            .count(),
        1
    );

    // A guild that comes back from an outage replaces its ID-only entry.
    assert_eq!(store.unavailable_guilds().count(), 1);
    apply(
        &mut store,
        "GUILD_CREATE",
        r#"{"id":"41771983423143940","name":"Back","owner_id":"1","channels":[{"id":"9","type":0}]}"#,
    );
    assert_eq!(store.unavailable_guilds().count(), 0);
    assert_eq!(
        store
            .guild(id(OUTAGE))
            .unwrap()
            .channel(id(9))
            .unwrap()
            .guild_id,
        Some(id(OUTAGE))
    );

    // An outage removes the details and remembers the ID; leaving removes it.
    apply(
        &mut store,
        "GUILD_DELETE",
        &format!(r#"{{"id":"{SECOND}","unavailable":true}}"#),
    );
    assert!(store.guild(id(SECOND)).is_none());
    assert!(store.unavailable_guilds().any(|g| g == id(SECOND)));
    apply(
        &mut store,
        "GUILD_DELETE",
        &format!(r#"{{"id":"{SECOND}"}}"#),
    );
    apply(
        &mut store,
        "GUILD_DELETE",
        &format!(r#"{{"id":"{SECOND}"}}"#),
    );
    assert!(!store.unavailable_guilds().any(|g| g == id(SECOND)));
    assert_eq!(store.guild_ids(), [id(FIRST), id(OUTAGE)]);

    // Channels: create, update (keeping the marker), delete, repeated.
    let create = format!(r#"{{"id":"77","type":0,"guild_id":"{FIRST}","name":"new"}}"#);
    apply(&mut store, "CHANNEL_CREATE", &create);
    apply(&mut store, "CHANNEL_CREATE", &create);
    assert_eq!(store.guild(id(FIRST)).unwrap().channels().count(), 2);
    let update = format!(r#"{{"id":"77","type":0,"guild_id":"{FIRST}","name":"renamed"}}"#);
    apply(&mut store, "CHANNEL_UPDATE", &update);
    assert_eq!(
        store
            .guild(id(FIRST))
            .unwrap()
            .channel(id(77))
            .unwrap()
            .name
            .as_deref(),
        Some("renamed")
    );
    let delete = format!(r#"{{"id":"77","type":0,"guild_id":"{FIRST}"}}"#);
    apply(&mut store, "CHANNEL_DELETE", &delete);
    apply(&mut store, "CHANNEL_DELETE", &delete);
    assert_eq!(store.guild(id(FIRST)).unwrap().channels().count(), 1);
    // A channel event for a guild we do not hold is dropped.
    let stray = r#"{"id":"78","type":0,"guild_id":"5"}"#;
    assert!(apply(&mut store, "CHANNEL_CREATE", stray).is_empty());
}

#[test]
fn direct_message_channels_are_normalized_and_track_their_last_message() {
    let mut store = ready_store();
    let users_before = store.users_held();
    let created = r#"{"id":"300000000000000009","type":1,"recipients":[{"id":"4242","username":"fresh","global_name":null,"avatar":null}]}"#;
    let changes = apply(&mut store, "CHANNEL_CREATE", created);
    assert!(changes.private_channels);
    let channel = store.private_channel(id(300_000_000_000_000_009)).unwrap();
    assert!(channel.recipients.is_empty());
    assert_eq!(channel.recipient_ids, [id(4242)]);
    assert_eq!(store.users_held(), users_before + 1);

    // A newer message moves the marker; an older replay does not.
    let Dispatch::MessageCreate(mut message) = decode_fixture("MESSAGE_CREATE", MESSAGE) else {
        panic!("MESSAGE_CREATE did not decode");
    };
    message.guild_id = None;
    message.channel_id = id(300_000_000_000_000_009);
    message.id = id(500);
    store.apply(event(Dispatch::MessageCreate(message.clone())));
    message.id = id(400);
    store.apply(event(Dispatch::MessageCreate(message)));
    assert_eq!(
        store
            .private_channel(id(300_000_000_000_000_009))
            .unwrap()
            .last_message_id,
        Some(id(500))
    );
    apply(
        &mut store,
        "CHANNEL_DELETE",
        r#"{"id":"300000000000000009","type":1}"#,
    );
    assert!(store.private_channel(id(300_000_000_000_000_009)).is_none());
    check(&store);
}

#[test]
fn guild_message_markers_advance_and_message_deletes_change_nothing_here() {
    let mut store = ready_store();
    let Dispatch::MessageCreate(mut message) = decode_fixture("MESSAGE_CREATE", MESSAGE) else {
        panic!("MESSAGE_CREATE did not decode");
    };
    message.guild_id = Some(id(FIRST));
    message.channel_id = id(GENERAL);
    message.id = id(175_928_847_299_117_900);
    let changes = store.apply(event(Dispatch::MessageCreate(message.clone())));
    assert_eq!(changes.guilds, BTreeSet::from([id(FIRST)]));
    assert_eq!(
        store
            .guild(id(FIRST))
            .unwrap()
            .channel(id(GENERAL))
            .unwrap()
            .last_message_id,
        Some(id(175_928_847_299_117_900))
    );
    // Replayed or older: no change reported.
    let changes = store.apply(event(Dispatch::MessageCreate(message)));
    assert!(changes.is_empty());
    let changes = store.apply(event(Dispatch::MessageDelete(MessageDelete {
        id: id(1),
        channel_id: id(GENERAL),
        guild_id: Some(id(FIRST)),
    })));
    assert!(changes.is_empty());
    check(&store);
}

// ----- retention -----

fn big_guild(guild: u64, channels: u64) -> Guild {
    Guild {
        id: id(guild),
        name: format!("Guild number {guild}"),
        icon: Some("0123456789abcdef0123456789abcdef".to_owned()),
        owner_id: Some(id(1)),
        roles: (0..12)
            .map(|n| Role {
                id: id(if n == 0 { guild } else { guild * 100 + n }),
                name: format!("role-{n}"),
                permissions: Permissions(1024),
                position: n as i32,
            })
            .collect(),
        channels: (0..channels)
            .map(|n| Channel {
                id: id(guild * 10_000 + n),
                kind: ChannelKind::GuildText,
                guild_id: Some(id(guild)),
                name: Some(format!("channel-{n}-name")),
                position: Some(n as i32),
                parent_id: None,
                permission_overwrites: vec![
                    PermissionOverwrite {
                        id: id(guild * 100),
                        kind: OverwriteKind::Role,
                        allow: Permissions(1024),
                        deny: Permissions(0),
                    };
                    2
                ],
                recipients: Vec::new(),
                recipient_ids: Vec::new(),
                last_message_id: Some(id(n + 1)),
            })
            .collect(),
        members: vec![GuildMember {
            user_id: id(ME),
            nick: None,
            roles: vec![id(guild * 100 + 1)],
            communication_disabled_until: None,
        }],
        member_count: 250_000,
    }
}

fn person(n: u64) -> User {
    User {
        id: id(1_000_000 + n),
        username: format!("someone-{n}"),
        global_name: Some(format!("Someone Number {n}")),
        avatar: Some("a_0123456789abcdef0123456789abcdef".to_owned()),
        bot: false,
    }
}

fn member_event(guild: u64, n: u64) -> GatewayEvent {
    event(Dispatch::GuildMemberUpdate(Box::new(GuildMemberEvent {
        guild_id: id(guild),
        member: GuildMember {
            user_id: id(1_000_000 + n),
            nick: Some(format!("nick-{n}")),
            roles: vec![id(guild * 100 + 1), id(guild * 100 + 2)],
            communication_disabled_until: None,
        },
        nick_present: true,
        roles_present: true,
        timeout_present: true,
        username_present: true,
        global_name_present: true,
        avatar_present: true,
        bot_present: true,
        users: vec![person(n)],
    })))
}

fn big_ready(guilds: u64, channels: u64) -> Ready {
    Ready {
        session_id: SessionId::new("big".to_owned()),
        user: User {
            id: id(ME),
            username: "alt".to_owned(),
            global_name: None,
            avatar: None,
            bot: false,
        },
        users: Vec::new(),
        guilds: (1..=guilds).map(|g| big_guild(g, channels)).collect(),
        unavailable_guilds: Vec::new(),
        private_channels: Vec::new(),
    }
}

#[test]
fn the_metadata_budget_holds_through_a_large_account_and_flood_of_members() {
    // 120 guilds with 250 channels each, plus 60,000 member updates (each with
    // its user): well over 12 MiB if nothing were ever shed.
    let mut store = Store::new();
    store.apply(event(Dispatch::Ready(Box::new(big_ready(120, 250)))));
    check(&store);
    let identity = store.bytes();
    assert!(
        identity < METADATA_BUDGET,
        "the fixture's floor must fit the budget"
    );

    // The voice participants and a requested member must survive everything.
    store.apply(event(Dispatch::VoiceStateUpdate(Box::new(
        VoiceStateUpdate {
            state: VoiceState {
                guild_id: Some(id(1)),
                channel_id: Some(id(10_000)),
                user_id: id(1_000_240),
                session_id: "v".to_owned(),
                deaf: false,
                mute: false,
                self_deaf: false,
                self_mute: false,
                self_stream: false,
                self_video: false,
                suppress: false,
            },
            member: None,
            users: Vec::new(),
        },
    ))));
    let mut target = select(1, 10_000);
    target.set_member_interest(id(1), [id(1_000_360)]);
    store.focus(&target);

    let mut peak = 0;
    for n in 0..60_000u64 {
        store.apply(member_event(1 + n % 120, n));
        peak = peak.max(store.bytes());
        if n % 7_919 == 0 {
            check(&store);
        }
    }
    check(&store);
    assert!(peak <= METADATA_BUDGET, "peak {peak} exceeded the budget");
    assert!(store.bytes() <= METADATA_BUDGET);
    assert!(!store.over_budget());

    // Navigable identity is intact: every guild, every channel, every role.
    assert_eq!(store.guild_ids().len(), 120);
    for guild in store.guild_ids() {
        let entry = store.guild(*guild).unwrap();
        assert_eq!(entry.channels().count(), 250, "guild {guild}");
        assert_eq!(entry.roles().len(), 12);
        assert!(
            entry.member(id(ME)).is_some(),
            "own member is permission data"
        );
    }
    // Pinned members survive; the oldest unreferenced ones went first.
    let first = store.guild(id(1)).unwrap();
    assert!(first.voice_state(id(1_000_240)).is_some());
    assert!(first.member(id(1_000_240)).is_some(), "voice participant");
    assert!(first.member(id(1_000_360)).is_some(), "requested member");
    assert!(store.user(id(1_000_240)).is_some());
    let held: usize = store
        .guild_ids()
        .iter()
        .map(|g| store.guild(*g).unwrap().members_held())
        .sum();
    assert!(held < 60_000, "something had to be shed");
    let newest = store.guild(id(1 + 59_999 % 120)).unwrap();
    assert!(
        newest.member(id(1_000_000 + 59_999)).is_some(),
        "the newest members are kept"
    );
}

#[test]
fn shedding_goes_oldest_unreferenced_first_and_never_touches_pinned_members() {
    // A budget that fits the identity but only a handful of members.
    let mut probe = Store::new();
    probe.apply(event(Dispatch::Ready(Box::new(big_ready(2, 20)))));
    let floor = probe.bytes();
    let mut store = Store::with_budget(floor + 20 * 400);
    store.apply(event(Dispatch::Ready(Box::new(big_ready(2, 20)))));

    // A voice participant with a member, and a requested member.
    store.apply(member_event(1, 1));
    store.apply(event(Dispatch::VoiceStateUpdate(Box::new(
        VoiceStateUpdate {
            state: VoiceState {
                guild_id: Some(id(1)),
                channel_id: Some(id(10_000)),
                user_id: id(1_000_001),
                session_id: "v".to_owned(),
                deaf: false,
                mute: false,
                self_deaf: false,
                self_mute: false,
                self_stream: false,
                self_video: false,
                suppress: false,
            },
            member: None,
            users: Vec::new(),
        },
    ))));
    store.apply(member_event(1, 2));
    let mut target = select(1, 10_000);
    target.set_member_interest(id(1), [id(1_000_002)]);
    store.focus(&target);

    for n in 3..400 {
        store.apply(member_event(1, n));
        assert!(store.bytes() <= store.budget(), "after member {n}");
    }
    check(&store);
    let first = store.guild(id(1)).unwrap();
    assert!(first.member(id(1_000_001)).is_some(), "voice participant");
    assert!(first.member(id(1_000_002)).is_some(), "requested member");
    assert!(first.member(id(ME)).is_some(), "the current user");
    assert!(first.member(id(1_000_399)).is_some(), "the newest write");
    assert!(
        first.member(id(1_000_003)).is_none(),
        "the oldest went first"
    );
    assert!(first.members_held() < 60);
    // Users nobody references any more went with their members.
    assert!(store.user(id(1_000_003)).is_none());
    assert!(store.user(id(1_000_399)).is_some());
}

#[test]
fn visible_list_rows_pin_their_members_until_navigation_releases_them() {
    let mut probe = Store::new();
    probe.apply(event(Dispatch::Ready(Box::new(big_ready(1, 10)))));
    let mut store = Store::with_budget(probe.bytes() + 12 * 400);
    store.apply(event(Dispatch::Ready(Box::new(big_ready(1, 10)))));
    store.focus(&select(1, 10_000));

    // A list showing members 1..=3, then a flood that forces shedding.
    let rows: Vec<ListRow> = (1..=3)
        .map(|n| ListRow::Member(id(1_000_000 + n)))
        .collect();
    store.apply(event(Dispatch::MemberListUpdate(Box::new(
        MemberListUpdate {
            guild_id: id(1),
            list_id: MemberListId("everyone".to_owned()),
            member_count: Some(250_000),
            online_count: Some(10),
            groups: Some(Vec::new()),
            ops: vec![MemberListOp::Sync {
                start: 0,
                end: 99,
                rows,
            }],
            members: (1..=3)
                .map(|n| GuildMember {
                    user_id: id(1_000_000 + n),
                    nick: None,
                    roles: Vec::new(),
                    communication_disabled_until: None,
                })
                .collect(),
            users: (1..=3).map(person).collect(),
        },
    ))));
    for n in 10..300 {
        store.apply(member_event(1, n));
    }
    check(&store);
    for n in 1..=3 {
        assert!(
            store
                .guild(id(1))
                .unwrap()
                .member(id(1_000_000 + n))
                .is_some(),
            "visible row {n}"
        );
        assert!(store.user(id(1_000_000 + n)).is_some());
    }

    // Leaving the channel releases the list, and the rows become ordinary.
    store.focus(&SubscriptionTarget::default());
    for n in 300..600 {
        store.apply(member_event(1, n));
    }
    check(&store);
    assert!(store.guild(id(1)).unwrap().member(id(1_000_001)).is_none());
    assert!(store.user(id(1_000_001)).is_none());
    assert!(store.bytes() <= store.budget());
}

#[test]
fn an_identity_floor_above_the_budget_terminates_and_releases_account_state() {
    let mut store = Store::with_budget(10_000);
    let changes = store.apply(event(Dispatch::Ready(Box::new(big_ready(3, 30)))));
    assert!(changes.reset);
    assert!(store.limit_exceeded());
    assert!(!store.over_budget());
    assert_eq!(store.bytes(), 0);
    assert!(store.guild_ids().is_empty());
    assert!(store.unavailable_guilds().next().is_none());
    assert!(store.current_user().is_none());
    assert_eq!(store.users_held(), 0);
    assert_eq!(store.private_channels().count(), 0);
    check(&store);

    assert!(store.focus(&select(1, 10_000)).is_empty());
    assert!(
        store
            .apply(event(Dispatch::Ready(Box::new(big_ready(1, 1)))))
            .is_empty()
    );
    assert!(store.apply(member_event(1, 1)).is_empty());
    assert!(store.limit_exceeded());
    assert_eq!(store.bytes(), 0);
    check(&store);
}

#[test]
fn channel_unread_markers_are_the_only_channel_detail_passive_updates_touch() {
    let mut store = ready_store();
    let before = store
        .guild(id(SECOND))
        .unwrap()
        .channel(id(LOBBY))
        .unwrap()
        .clone();
    store.apply(event(Dispatch::PassiveUpdate(Box::new(PassiveUpdate {
        guild_id: id(SECOND),
        channels: vec![ChannelUnread {
            id: id(LOBBY),
            last_message_id: Some(id(900)),
        }],
        updated_voice_states: Vec::new(),
        removed_voice_states: Vec::new(),
        members: Vec::new(),
        users: Vec::new(),
    }))));
    let after = store.guild(id(SECOND)).unwrap().channel(id(LOBBY)).unwrap();
    assert_eq!(after.last_message_id, Some(id(900)));
    assert_eq!(after.name, before.name);
    assert_eq!(after.permission_overwrites, before.permission_overwrites);
    check(&store);
}

#[test]
fn connection_states_and_resumed_change_nothing_and_debug_hides_content() {
    let mut store = ready_store();
    assert!(
        store
            .apply(GatewayEvent::State(crate::gateway::ConnectionState::Ready))
            .is_empty()
    );
    assert!(store.apply(event(Dispatch::Resumed)).is_empty());
    assert!(
        store
            .apply(event(Dispatch::GuildDelete(GuildDelete {
                id: id(5),
                unavailable: false
            })))
            .guilds
            .contains(&id(5))
    );
    let shown = format!("{store:?}");
    assert!(!shown.contains("Test Server") && !shown.contains("alt_fixture"));
}

// ----- allocation accounting -----

fn allocated_text(value: &str, capacity: usize) -> String {
    let mut text = String::with_capacity(capacity);
    text.push_str(value);
    text
}

#[test]
fn entity_costs_charge_owned_capacity_instead_of_logical_lengths() {
    let user = User {
        id: id(1),
        username: allocated_text("u", 2_048),
        global_name: Some(allocated_text("g", 1_024)),
        avatar: Some(allocated_text("a", 512)),
        bot: false,
    };
    assert_eq!(
        user_bytes(&user),
        size_of::<User>()
            + user.username.capacity()
            + user.global_name.as_ref().unwrap().capacity()
            + user.avatar.as_ref().unwrap().capacity()
    );

    let role = Role {
        id: id(2),
        name: allocated_text("r", 256),
        permissions: Permissions(0),
        position: 0,
    };
    assert_eq!(role_bytes(&role), size_of::<Role>() + role.name.capacity());

    let member = GuildMember {
        user_id: id(1),
        nick: Some(allocated_text("n", 1_024)),
        roles: Vec::with_capacity(64),
        communication_disabled_until: Some(allocated_text("t", 256)),
    };
    assert_eq!(
        member_bytes(&member),
        size_of::<MemberSlot>()
            + member.nick.as_ref().unwrap().capacity()
            + member.roles.capacity() * size_of::<Snowflake>()
            + member
                .communication_disabled_until
                .as_ref()
                .unwrap()
                .capacity()
    );

    let mut channel = big_guild(1, 1).channels.pop().unwrap();
    channel.name = Some(allocated_text("c", 512));
    channel.permission_overwrites.reserve(32);
    channel.recipient_ids = Vec::with_capacity(128);
    channel.recipients = Vec::with_capacity(16);
    channel.recipients.push(user);
    assert_eq!(
        channel_bytes(&channel),
        size_of::<Channel>()
            + channel.name.as_ref().unwrap().capacity()
            + channel.permission_overwrites.capacity() * size_of::<PermissionOverwrite>()
            + channel.recipient_ids.capacity() * size_of::<Snowflake>()
            + channel.recipients.capacity() * size_of::<User>()
            + channel.recipients[0].username.capacity()
            + channel.recipients[0]
                .global_name
                .as_ref()
                .unwrap()
                .capacity()
            + channel.recipients[0].avatar.as_ref().unwrap().capacity()
    );

    let voice = VoiceState {
        guild_id: Some(id(1)),
        channel_id: Some(id(10_000)),
        user_id: id(1),
        session_id: allocated_text("v", 4_096),
        deaf: false,
        mute: false,
        self_deaf: false,
        self_mute: false,
        self_stream: false,
        self_video: false,
        suppress: false,
    };
    assert_eq!(
        voice_bytes(&voice),
        size_of::<VoiceState>() + voice.session_id.capacity()
    );
}

#[test]
fn store_costs_include_empty_table_slack_order_unavailable_and_focus_allocations() {
    let mut store = Store::new();
    store.users.reserve(1_024);
    store.guilds.reserve(64);
    store.private_channels.reserve(128);
    store.guild_order.reserve(2_048);
    store.unavailable.extend((1..=24).map(id));
    store.focus.asked.reserve(16);
    let mut members = HashSet::with_capacity(512);
    members.insert(id(99));
    store.focus.asked.insert(id(1), members);
    store.focus.list_id = Some(MemberListId(allocated_text("everyone", 4_096)));
    store.focus.ranges = Vec::with_capacity(64);
    store.focus.ranges.push((0, 99));

    let expected = table_bytes::<Snowflake, User>(store.users.capacity())
        + table_bytes::<Snowflake, GuildEntry>(store.guilds.capacity())
        + table_bytes::<Snowflake, Channel>(store.private_channels.capacity())
        + store.guild_order.capacity() * size_of::<Snowflake>()
        + store.unavailable.len() * (size_of::<Snowflake>() + 48)
        + table_bytes::<Snowflake, HashSet<Snowflake>>(store.focus.asked.capacity())
        + table_bytes::<Snowflake, ()>(store.focus.asked[&id(1)].capacity())
        + store.focus.list_id.as_ref().unwrap().0.capacity()
        + store.focus.ranges.capacity() * size_of::<crate::gateway::MemberRange>();
    assert_eq!(store.bytes(), expected);
    assert!(
        store.bytes() > 100_000,
        "reserved empty storage is not free"
    );
    check(&store);
    assert_eq!(
        table_bytes::<Snowflake, User>(3),
        4 * (size_of::<(Snowflake, User)>() + 1) + 16
    );
    assert_eq!(
        table_bytes::<Snowflake, User>(224),
        256 * (size_of::<(Snowflake, User)>() + 1) + 16
    );
}

#[test]
fn member_list_costs_include_id_header_and_row_group_allocations() {
    // Unknown groups are an opaque unit variant, not an owned String.
    let mut list = MemberList::new(MemberListId(allocated_text("everyone", 2_048)));
    let empty = list.bytes();
    assert_eq!(empty, size_of::<MemberList>() + list.id().0.capacity());
    let mut groups = Vec::with_capacity(256);
    groups.push(crate::gateway::ListGroup {
        id: GroupId::Other,
        count: 1,
    });
    let group_capacity = groups.capacity();
    list.set_header(Some(1), Some(0), Some(groups));
    assert_eq!(
        list.bytes() - empty,
        group_capacity * size_of::<crate::gateway::ListGroup>()
    );
    list.apply(MemberListOp::Sync {
        start: 0,
        end: 0,
        rows: vec![ListRow::Group(crate::gateway::ListGroup {
            id: GroupId::Other,
            count: 1,
        })],
    });
    assert!(
        list.bytes()
            >= empty
                + group_capacity * size_of::<crate::gateway::ListGroup>()
                + size_of::<ListRow>()
    );
    assert_eq!(list.bytes(), list.recount());
    list.clear_rows();
    assert_eq!(
        list.bytes(),
        empty + group_capacity * size_of::<crate::gateway::ListGroup>()
    );
    list.set_header(None, None, Some(Vec::new()));
    assert_eq!(list.bytes(), empty);
}

#[test]
fn compacting_unused_optional_table_capacity_preserves_live_members_and_users() {
    let mut store = Store::new();
    store.apply(event(Dispatch::Ready(Box::new(big_ready(1, 10)))));
    for n in 0..64 {
        store.apply(member_event(1, n));
    }
    let held = store.guild(id(1)).unwrap().members_held();
    let users = store.users_held();
    let compact = store.bytes();
    store.users.reserve(16_384);
    store.with_guild(id(1), |entry| {
        entry.members.reserve(16_384);
        entry.voice.reserve(16_384);
    });
    let user_capacity = store.users.capacity();
    let member_capacity = store.guild(id(1)).unwrap().members.capacity();
    let voice_capacity = store.guild(id(1)).unwrap().voice.capacity();
    assert!(store.bytes() > compact);
    check(&store);

    store.budget = compact + 1_024;
    store.apply(event(Dispatch::Resumed));
    assert!(!store.limit_exceeded());
    assert_eq!(store.guild(id(1)).unwrap().members_held(), held);
    assert_eq!(store.users_held(), users);
    assert!(store.users.capacity() < user_capacity);
    assert!(store.guild(id(1)).unwrap().members.capacity() < member_capacity);
    assert!(store.guild(id(1)).unwrap().voice.capacity() < voice_capacity);
    assert_eq!(store.guild(id(1)).unwrap().voice.capacity(), 0);
    assert!(store.bytes() <= store.budget());
    check(&store);
}

#[test]
fn eviction_releases_optional_map_allocations_and_keeps_the_ledger_exact() {
    let mut store = Store::new();
    store.apply(event(Dispatch::Ready(Box::new(big_ready(1, 10)))));
    let floor = store.bytes();
    for n in 0..512 {
        store.apply(member_event(1, n));
    }
    let user_capacity = store.users.capacity();
    let member_capacity = store.guild(id(1)).unwrap().members.capacity();
    store.budget = floor + 4_096;
    store.apply(event(Dispatch::Resumed));
    assert!(!store.limit_exceeded());
    assert!(store.guild(id(1)).unwrap().members_held() < 513);
    assert!(store.users.capacity() < user_capacity);
    assert!(store.guild(id(1)).unwrap().members.capacity() < member_capacity);
    assert!(store.guild(id(1)).unwrap().member(id(ME)).is_some());
    assert!(store.guild(id(1)).unwrap().member(id(1_000_511)).is_some());
    assert!(store.bytes() <= store.budget());
    check(&store);
}

#[test]
fn focus_allocation_growth_is_subject_to_the_same_terminal_limit() {
    let mut store = ready_store();
    store.apply(event(Dispatch::ReadySupplemental(Box::new(
        ReadySupplemental {
            guilds: Vec::new(),
            users: Vec::new(),
            lazy_private_channels: Vec::new(),
        },
    ))));
    store.budget = store.bytes();
    let mut target = select(FIRST, GENERAL);
    target.set_member_interest(id(FIRST), (0..200).map(|n| id(2_000_000 + n)));
    let changes = store.focus(&target);
    assert!(changes.reset);
    assert!(store.limit_exceeded());
    assert_eq!(store.bytes(), 0);
    assert!(store.focus(&SubscriptionTarget::default()).is_empty());
    assert!(store.apply(event(Dispatch::Resumed)).is_empty());
    check(&store);
}

#[test]
fn channel_snapshots_keep_latest_duplicate_identity_in_compact_sorted_storage() {
    let mut ready = big_ready(1, 3);
    let guild = &mut ready.guilds[0];
    guild.channels.reverse();
    let mut latest = guild.channels[2].clone();
    let duplicate = latest.id;
    latest.name = Some("latest snapshot".to_owned());
    latest.last_message_id = Some(id(900));
    guild.channels.reserve(2_048);
    guild.channels.push(latest);
    let mut store = Store::new();
    store.apply(event(Dispatch::Ready(Box::new(ready))));
    let entry = store.guild(id(1)).unwrap();
    assert_eq!(entry.channels.len(), 3);
    assert_eq!(entry.channels.capacity(), 3);
    assert_eq!(
        entry.channel(duplicate).unwrap().name.as_deref(),
        Some("latest snapshot")
    );
    assert_eq!(
        entry.channel(duplicate).unwrap().last_message_id,
        Some(id(900))
    );
    assert!(
        entry
            .channels
            .windows(2)
            .all(|pair| pair[0].id < pair[1].id)
    );
    check(&store);

    let mut new = entry.channel(duplicate).unwrap().clone();
    new.id = id(9_999);
    store.apply(event(Dispatch::ChannelCreate(Box::new(new.clone()))));
    assert!(store.guild(id(1)).unwrap().channel(new.id).is_some());
    check(&store);
    store.apply(event(Dispatch::ChannelDelete(Box::new(new))));
    assert_eq!(store.guild(id(1)).unwrap().channels.len(), 3);
    assert!(store.guild(id(1)).unwrap().channel(id(9_999)).is_none());
    check(&store);
}
