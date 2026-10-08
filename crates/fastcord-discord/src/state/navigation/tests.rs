use fastcord_model::{Guild, GuildMember, PermissionOverwrite, Role, User};
use serde_json::{Value, json};

use super::*;
use crate::gateway::{
    Dispatch, GatewayEvent, GuildCreate, GuildDelete, Ready, SessionId, decode_fixture,
};

const READY: &str = include_str!("../../../../../fixtures/gateway/ready.json");
const NAV_READY: &str = include_str!("../../../../../fixtures/gateway/navigation_ready.json");
const ROLE_EVENTS: &str = include_str!("../../../../../fixtures/gateway/guild_role_events.json");
const LIST_SYNC: &str =
    include_str!("../../../../../fixtures/gateway/member_list_update_sync.json");

const ME: u64 = 175_928_847_299_117_063;
const FIRST: u64 = 41_771_983_423_143_937;
const SECOND: u64 = 41_771_983_423_143_941;
const OUTAGE: u64 = 41_771_983_423_143_940;
const GENERAL: u64 = 41_771_983_423_143_938;
const VOICE: u64 = 41_771_983_423_143_939;
const LOBBY: u64 = 41_771_983_423_143_942;

fn id(value: u64) -> Snowflake {
    Snowflake(value)
}

fn event(event: Dispatch) -> GatewayEvent {
    GatewayEvent::Dispatch { sequence: 1, event }
}

fn apply(store: &mut Store, name: &str, raw: &str) -> super::super::Changes {
    let changes = store.apply(event(decode_fixture(name, raw)));
    assert_eq!(store.bytes(), store.recount());
    changes
}

fn ready(raw: &str) -> Ready {
    let Dispatch::Ready(ready) = decode_fixture("READY", raw) else {
        panic!("READY did not decode");
    };
    *ready
}

fn replace(store: &mut Store, ready: Ready) {
    store.apply(event(Dispatch::Ready(Box::new(ready))));
    assert!(!store.limit_exceeded());
    assert_eq!(store.bytes(), store.recount());
}

fn setup(raw: &str) -> (Store, Navigation) {
    let mut store = Store::new();
    replace(&mut store, ready(raw));
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    (store, navigation)
}

fn rows(navigation: &Navigation, store: &Store) -> Vec<ChannelRow> {
    navigation.channel_window(store, 0, usize::MAX).rows
}

fn ids(rows: &[ChannelRow]) -> Vec<Snowflake> {
    rows.iter().map(|row| row.id).collect()
}

fn own_roles(store: &mut Store, roles: &[u64]) {
    apply(
        store,
        "GUILD_MEMBER_UPDATE",
        &json!({
            "guild_id": "100", "user": {"id": "1"},
            "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>()
        })
        .to_string(),
    );
}

fn update_channel(store: &mut Store, channel: Channel) {
    let changes = store.apply(event(Dispatch::ChannelUpdate(Box::new(channel))));
    assert!(!changes.guilds.is_empty());
    assert_eq!(store.bytes(), store.recount());
}

#[test]
fn normalized_ready_exposes_available_guilds_and_accessible_text_voice_only() {
    let (store, mut navigation) = setup(READY);
    let snapshot = navigation.snapshot(&store, 0, usize::MAX, 0, usize::MAX);
    assert_eq!(snapshot.selected_guild, Some(id(FIRST)));
    assert!(snapshot.selection.is_none(), "no channel auto-opens");
    assert!(!snapshot.permissions_pending);
    assert_eq!(snapshot.guilds.total, 3);
    assert_eq!(
        snapshot
            .guilds
            .rows
            .iter()
            .map(|row| (row.id, row.available))
            .collect::<Vec<_>>(),
        [(id(FIRST), true), (id(SECOND), true), (id(OUTAGE), false)]
    );
    assert_eq!(snapshot.guilds.rows[2].name, OUTAGE.to_string());
    assert_eq!(ids(&snapshot.channels.rows), [id(GENERAL), id(VOICE)]);
    assert_eq!(snapshot.channels.rows[0].kind, ChannelKind::Text);
    assert_eq!(snapshot.channels.rows[1].kind, ChannelKind::Voice);
    assert!(snapshot.channels.rows.iter().all(|row| row.enabled));

    assert!(navigation.select_channel(&store, id(VOICE)));
    let selection = navigation.selection(&store).unwrap();
    assert_eq!(
        (selection.guild_id, selection.channel_id),
        (id(FIRST), id(VOICE))
    );
    assert!(selection.permissions.connect && selection.permissions.speak);
    assert!(selection.permissions.read_history && selection.permissions.send_messages);

    let target = navigation.subscription_target(&store);
    assert_eq!(target.selected_guild(), Some(id(FIRST)));
    assert_eq!(target.selected_channel(), Some(id(VOICE)));
    assert!(target.ranges().is_empty());
    assert!(target.guilds()[&id(FIRST)].channels.is_empty());
    assert_eq!(target.member_interest(id(FIRST)).count(), 0);
}

#[test]
fn category_groups_positions_and_id_ties_have_deterministic_order() {
    let (mut store, navigation) = setup(NAV_READY);
    let before = rows(&navigation, &store);
    assert_eq!(
        ids(&before),
        [
            1000, 1001, 2201, 2301, 3000, 3002, 3003, 3004, 2000, 2002, 2003, 2001, 2100, 2101,
            2400,
        ]
        .map(id)
    );
    assert_eq!(before.len(), 15);
    assert!(
        before
            .iter()
            .filter(|row| row.kind == ChannelKind::Category)
            .all(|row| !row.enabled)
    );
    assert!(
        before
            .iter()
            .all(|row| !row.name.contains("Forbidden Category"))
    );
    assert!(
        before
            .iter()
            .find(|row| row.id == id(2201))
            .unwrap()
            .parent_id
            .is_none()
    );
    assert!(
        before
            .iter()
            .find(|row| row.id == id(2301))
            .unwrap()
            .parent_id
            .is_none()
    );
    assert_eq!(
        before
            .iter()
            .find(|row| row.id == id(2001))
            .unwrap()
            .parent_id,
        Some(id(2000))
    );
    let blocked = before.iter().find(|row| row.id == id(3003)).unwrap();
    assert!(blocked.permissions.view_channel);
    assert!(!blocked.permissions.connect && !blocked.enabled);
    assert_eq!(
        before.iter().find(|row| row.id == id(2001)).unwrap().kind,
        ChannelKind::Announcement
    );
    assert_eq!(
        before.iter().find(|row| row.id == id(3004)).unwrap().kind,
        ChannelKind::Voice
    );

    let mut shuffled = ready(NAV_READY);
    shuffled.guilds[0].channels.reverse();
    replace(&mut store, shuffled);
    assert_eq!(
        rows(&navigation, &store),
        before,
        "wire array order cannot move rows"
    );
}

#[test]
fn injected_hidden_disabled_category_unknown_and_foreign_ids_are_rejected() {
    let (store, mut navigation) = setup(NAV_READY);
    assert!(navigation.select_channel(&store, id(1000)));
    for forbidden in [1002, 3001, 2200, 2000, 3003, 2501, 2502, 9999] {
        assert!(
            !navigation.select_channel(&store, id(forbidden)),
            "{forbidden}"
        );
        assert_eq!(navigation.selected_channel(), Some(id(1000)));
        assert_eq!(navigation.remembered, BTreeMap::from([(id(100), id(1000))]));
    }
    assert!(!navigation.select_guild(&store, id(9999)));
    assert_eq!(navigation.selected_guild(), Some(id(100)));

    let (store, mut navigation) = setup(READY);
    assert!(navigation.select_channel(&store, id(GENERAL)));
    for forbidden in [LOBBY, 300_000_000_000_000_001, 9999] {
        assert!(!navigation.select_channel(&store, id(forbidden)));
        assert_eq!(navigation.selected_channel(), Some(id(GENERAL)));
    }
    assert!(!navigation.select_guild(&store, id(OUTAGE)));
    assert_eq!(navigation.selected_guild(), Some(id(FIRST)));
}

#[test]
fn stable_ids_survive_updates_ready_reorder_and_per_guild_navigation() {
    let (mut store, mut navigation) = setup(READY);
    assert!(navigation.select_channel(&store, id(VOICE)));
    assert!(navigation.select_guild(&store, id(SECOND)));
    assert!(navigation.selected_channel().is_none());
    assert!(navigation.select_channel(&store, id(LOBBY)));
    assert!(navigation.select_guild(&store, id(FIRST)));
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));

    let mut voice = store
        .guild(id(FIRST))
        .unwrap()
        .channel(id(VOICE))
        .unwrap()
        .clone();
    voice.name = Some("Renamed voice".to_owned());
    voice.position = Some(-5);
    update_channel(&mut store, voice);
    navigation.reconcile(&store);
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));
    assert_eq!(navigation.selection(&store).unwrap().name, "Renamed voice");
    assert_eq!(ids(&rows(&navigation, &store)), [id(VOICE), id(GENERAL)]);

    let mut fresh = ready(READY);
    fresh.guilds.reverse();
    replace(&mut store, fresh);
    navigation.reconcile(&store);
    assert_eq!(navigation.selected_guild(), Some(id(FIRST)));
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));
    assert!(navigation.select_guild(&store, id(SECOND)));
    assert_eq!(navigation.selected_channel(), Some(id(LOBBY)));
}

#[test]
fn member_roles_and_channel_overwrites_revoke_open_selection_without_fallback() {
    let (mut store, mut navigation) = setup(NAV_READY);
    assert!(
        navigation.select_channel(&store, id(3000)),
        "role allow wins combined deny"
    );
    own_roles(&mut store, &[101]);
    assert!(
        navigation.selection(&store).is_none(),
        "queries recheck before reconcile"
    );
    assert!(!navigation.select_channel(&store, id(3000)));
    navigation.reconcile(&store);
    assert!(navigation.selected_channel().is_none());
    assert!(!navigation.remembered.contains_key(&id(100)));
    assert!(!ids(&rows(&navigation, &store)).contains(&id(3000)));

    own_roles(&mut store, &[101, 102]);
    navigation.reconcile(&store);
    assert!(
        navigation.selected_channel().is_none(),
        "restored access is not an implicit open"
    );
    assert!(navigation.select_channel(&store, id(3000)));
    let mut channel = store
        .guild(id(100))
        .unwrap()
        .channel(id(3000))
        .unwrap()
        .clone();
    channel.permission_overwrites.push(PermissionOverwrite {
        id: id(1),
        kind: fastcord_model::OverwriteKind::Member,
        allow: Permissions::NONE,
        deny: Permissions::VIEW_CHANNEL,
    });
    update_channel(&mut store, channel);
    navigation.reconcile(&store);
    assert!(
        navigation.selected_channel().is_none(),
        "member deny wins role allow"
    );
    assert!(!navigation.select_channel(&store, id(3000)));

    assert!(
        navigation.select_channel(&store, id(3002)),
        "read-only text can open"
    );
    let summary = navigation.selection(&store).unwrap().permissions;
    assert!(summary.view_channel && summary.read_history);
    assert!(!summary.send_messages);
}

#[test]
fn role_create_update_delete_fixtures_change_permissions_and_keep_ledger_exact() {
    let (mut store, mut navigation) = setup(NAV_READY);
    assert!(navigation.select_channel(&store, id(1001)));
    let events: Vec<Value> = serde_json::from_str(ROLE_EVENTS).unwrap();
    for (index, dispatch) in events.iter().enumerate() {
        let changes = apply(
            &mut store,
            dispatch["t"].as_str().unwrap(),
            &dispatch["d"].to_string(),
        );
        assert!(changes.guilds.contains(&id(100)));
        navigation.reconcile(&store);
        if index == 0 {
            assert!(
                store
                    .guild(id(100))
                    .unwrap()
                    .roles()
                    .iter()
                    .any(|role| role.id == id(103))
            );
            assert_eq!(navigation.selected_channel(), Some(id(1001)));
            apply(&mut store, "GUILD_ROLE_CREATE", &dispatch["d"].to_string());
            assert_eq!(
                store
                    .guild(id(100))
                    .unwrap()
                    .roles()
                    .iter()
                    .filter(|role| role.id == id(103))
                    .count(),
                1
            );
        } else {
            assert!(navigation.selected_channel().is_none());
            assert!(!navigation.select_channel(&store, id(1001)));
        }
    }
    let guild = store.guild(id(100)).unwrap();
    assert!(!guild.roles().iter().any(|role| role.id == id(101)));
    assert!(!guild.member(id(1)).unwrap().roles.contains(&id(101)));
    assert!(
        guild.channels().all(
            |channel| channel.permission_overwrites.iter().all(|overwrite| {
                overwrite.kind != fastcord_model::OverwriteKind::Role || overwrite.id != id(101)
            })
        )
    );
    assert!(
        !navigation
            .snapshot(&store, 0, 10, 0, 10)
            .permissions_pending
    );
    assert!(apply(&mut store, "GUILD_ROLE_DELETE", &events[2]["d"].to_string()).is_empty());
    assert!(
        apply(
            &mut store,
            "GUILD_ROLE_CREATE",
            r#"{"guild_id":"999","role":{"id":"103","name":"unknown guild","permissions":"8"}}"#,
        )
        .is_empty()
    );
    assert_eq!(store.guild_ids(), [id(100)]);
}

#[test]
fn partial_guild_updates_preserve_selection_but_owner_loss_rechecks_permissions() {
    let (mut store, mut navigation) = setup(READY);
    assert!(navigation.select_guild(&store, id(SECOND)));
    assert!(navigation.select_channel(&store, id(LOBBY)));
    let before_roles = store.guild(id(SECOND)).unwrap().roles().to_vec();
    apply(
        &mut store,
        "GUILD_UPDATE",
        &json!({"id": SECOND.to_string(), "name": "Renamed guild", "member_count": 10}).to_string(),
    );
    navigation.reconcile(&store);
    let guild = store.guild(id(SECOND)).unwrap();
    assert_eq!(guild.name(), "Renamed guild");
    assert_eq!(guild.member_count(), 10);
    assert_eq!(guild.owner_id(), Some(id(ME)));
    assert_eq!(guild.icon(), Some("a_icon_hash"));
    assert_eq!(guild.roles(), before_roles);
    assert_eq!(navigation.selected_channel(), Some(id(LOBBY)));

    apply(
        &mut store,
        "GUILD_UPDATE",
        &json!({
            "id": SECOND.to_string(), "owner_id": null, "icon": null
        })
        .to_string(),
    );
    navigation.reconcile(&store);
    assert!(store.guild(id(SECOND)).unwrap().owner_id().is_none());
    assert!(store.guild(id(SECOND)).unwrap().icon().is_none());
    assert!(navigation.selected_channel().is_none());
    assert!(
        navigation
            .snapshot(&store, 0, 10, 0, 10)
            .permissions_pending
    );
    assert!(!navigation.select_channel(&store, id(LOBBY)));

    apply(
        &mut store,
        "GUILD_UPDATE",
        &json!({
            "id": SECOND.to_string(), "owner_id": ME.to_string(), "roles": null
        })
        .to_string(),
    );
    navigation.reconcile(&store);
    assert!(store.guild(id(SECOND)).unwrap().roles().is_empty());
    assert!(
        navigation.select_channel(&store, id(LOBBY)),
        "verified owner still bypasses missing roles"
    );
    assert!(
        apply(
            &mut store,
            "GUILD_UPDATE",
            r#"{"id":"9999","name":"Not joined"}"#
        )
        .is_empty()
    );
    assert_eq!(store.guild_ids().len(), 2);
}

#[test]
fn duplicate_role_snapshot_ids_are_last_write_wins_not_stale_admin_grants() {
    let (mut store, mut navigation) = setup(NAV_READY);
    let mut duplicated = ready(NAV_READY);
    duplicated.guilds[0].roles.insert(
        0,
        Role {
            id: id(101),
            name: "obsolete admin grant".to_owned(),
            permissions: Permissions::ADMINISTRATOR,
            position: 1,
        },
    );
    replace(&mut store, duplicated);
    navigation.reconcile(&store);
    assert_eq!(store.guild(id(100)).unwrap().roles().len(), 3);
    assert!(!navigation.select_channel(&store, id(1002)));
    assert!(!navigation.select_channel(&store, id(3003)));
}

#[test]
fn deleting_an_unknown_role_removes_orphaned_member_and_overwrite_references() {
    let (mut store, mut navigation) = setup(NAV_READY);
    let mut incomplete = ready(NAV_READY);
    incomplete.guilds[0].roles.retain(|role| role.id != id(101));
    replace(&mut store, incomplete);
    navigation.reconcile(&store);
    assert_eq!(navigation.channel_window(&store, 0, 128).total, 0);
    assert!(
        !apply(
            &mut store,
            "GUILD_ROLE_DELETE",
            r#"{"guild_id":"100","role_id":"101"}"#
        )
        .is_empty()
    );
    navigation.reconcile(&store);
    assert!(
        !navigation
            .snapshot(&store, 0, 10, 0, 10)
            .permissions_pending
    );
    assert!(navigation.select_channel(&store, id(1000)));
    assert!(!navigation.select_channel(&store, id(1001)));
}

#[test]
fn default_role_update_releases_an_obsolete_member_list_key() {
    let (mut store, _) = setup(READY);
    let mut target = SubscriptionTarget::default();
    target.select_channel(id(FIRST), id(GENERAL));
    store.focus(&target);
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST_SYNC);
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_some());
    let changes = apply(
        &mut store,
        "GUILD_ROLE_UPDATE",
        &json!({
            "guild_id": FIRST.to_string(),
            "role": {"id": FIRST.to_string(), "name": "@everyone", "permissions": "0"}
        })
        .to_string(),
    );
    assert!(changes.member_lists.contains(&id(FIRST)));
    assert!(store.guild(id(FIRST)).unwrap().member_list().is_none());
}

#[test]
fn incomplete_own_member_roles_and_owner_fail_closed_unless_owner_is_verified() {
    for missing in 0..5 {
        let (mut store, mut navigation) = setup(NAV_READY);
        assert!(navigation.select_channel(&store, id(1000)));
        let mut incomplete = ready(NAV_READY);
        let guild = &mut incomplete.guilds[0];
        match missing {
            0 => guild.members.clear(),
            1 => guild.roles.clear(),
            2 => guild.owner_id = None,
            3 => guild.roles.retain(|role| role.id != id(102)),
            4 => guild.members[0].roles_known = false,
            _ => unreachable!(),
        }
        replace(&mut store, incomplete);
        navigation.reconcile(&store);
        let snapshot = navigation.snapshot(&store, 0, 10, 0, 10);
        assert!(snapshot.permissions_pending, "missing case {missing}");
        assert_eq!(snapshot.channels.total, 0);
        assert!(navigation.selected_channel().is_none());
        assert!(!navigation.select_channel(&store, id(1000)));
        let target = navigation.subscription_target(&store);
        assert_eq!(target.selected_guild(), Some(id(100)));
        assert!(target.ranges().is_empty());
        assert_eq!(
            target.member_interest(id(100)).count(),
            usize::from(matches!(missing, 0 | 4))
        );
    }

    let (mut store, mut navigation) = setup(NAV_READY);
    let mut owned = ready(NAV_READY);
    owned.guilds[0].owner_id = Some(id(1));
    owned.guilds[0].roles.clear();
    owned.guilds[0].members.clear();
    replace(&mut store, owned);
    navigation.reconcile(&store);
    assert!(
        !navigation
            .snapshot(&store, 0, 10, 0, 10)
            .permissions_pending
    );
    assert!(
        navigation.select_channel(&store, id(1002)),
        "verified owner bypasses overwrites"
    );
    assert!(
        navigation.select_channel(&store, id(3003)),
        "verified owner has CONNECT"
    );
    assert_eq!(
        navigation
            .subscription_target(&store)
            .member_interest(id(100))
            .count(),
        0
    );
}

#[test]
fn omitted_null_and_explicit_empty_member_roles_are_not_conflated() {
    let original: Value = serde_json::from_str(NAV_READY).unwrap();
    for unknown in [true, false] {
        let mut payload = original.clone();
        let member = payload["merged_members"][0][0].as_object_mut().unwrap();
        if unknown {
            member.remove("roles");
        } else {
            member.insert("roles".to_owned(), Value::Null);
        }
        let (mut store, mut navigation) = setup(&payload.to_string());
        assert!(
            !store
                .guild(id(100))
                .unwrap()
                .member(id(1))
                .unwrap()
                .roles_known
        );
        assert_eq!(navigation.channel_window(&store, 0, 10).total, 0);
        assert_eq!(
            navigation
                .subscription_target(&store)
                .member_interest(id(100))
                .collect::<Vec<_>>(),
            [id(1)]
        );
        own_roles(&mut store, &[]);
        navigation.reconcile(&store);
        assert!(
            store
                .guild(id(100))
                .unwrap()
                .member(id(1))
                .unwrap()
                .roles_known
        );
        assert!(navigation.select_channel(&store, id(1000)));
        assert!(
            !navigation.select_channel(&store, id(1001)),
            "empty roles do not grant CONNECT"
        );
        assert_eq!(
            navigation
                .subscription_target(&store)
                .member_interest(id(100))
                .count(),
            0
        );
    }
}

#[test]
fn removing_own_member_fails_closed_and_requests_only_own_interest() {
    let (mut store, mut navigation) = setup(NAV_READY);
    assert!(navigation.select_channel(&store, id(1000)));
    apply(
        &mut store,
        "GUILD_MEMBER_REMOVE",
        r#"{"guild_id":"100","user":{"id":"1"}}"#,
    );
    navigation.reconcile(&store);
    let snapshot = navigation.snapshot(&store, 0, 10, 0, 10);
    assert!(snapshot.permissions_pending);
    assert!(navigation.selected_channel().is_none());
    assert_eq!(snapshot.channels.total, 0);
    let target = navigation.subscription_target(&store);
    assert_eq!(target.guilds().len(), 1);
    assert!(target.guilds()[&id(100)].channels.is_empty());
    assert_eq!(target.member_interest(id(100)).collect::<Vec<_>>(), [id(1)]);
}

#[test]
fn outages_disable_guilds_restore_only_valid_ids_and_deletes_prune_memory() {
    let (mut store, mut navigation) = setup(READY);
    assert!(navigation.select_channel(&store, id(VOICE)));
    apply(
        &mut store,
        "GUILD_DELETE",
        &json!({"id": FIRST.to_string(), "unavailable": true}).to_string(),
    );
    navigation.reconcile(&store);
    assert_eq!(navigation.selected_guild(), Some(id(FIRST)));
    assert!(navigation.selected_channel().is_none());
    assert_eq!(navigation.remembered.get(&id(FIRST)), Some(&id(VOICE)));
    assert!(navigation.subscription_target(&store).guilds().is_empty());
    let outage = navigation
        .guild_window(&store, 0, 10)
        .rows
        .into_iter()
        .find(|row| row.id == id(FIRST))
        .unwrap();
    assert!(!outage.available && outage.selected);
    assert!(!navigation.select_guild(&store, id(FIRST)));

    let guild = ready(READY).guilds.remove(0);
    store.apply(event(Dispatch::GuildCreate(Box::new(
        GuildCreate::Available {
            guild,
            users: Vec::new(),
        },
    ))));
    assert_eq!(store.bytes(), store.recount());
    navigation.reconcile(&store);
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));
    let voice = store
        .guild(id(FIRST))
        .unwrap()
        .channel(id(VOICE))
        .unwrap()
        .clone();
    store.apply(event(Dispatch::ChannelDelete(Box::new(voice))));
    navigation.reconcile(&store);
    assert!(navigation.selected_channel().is_none());
    assert!(!navigation.remembered.contains_key(&id(FIRST)));
    assert!(!navigation.select_channel(&store, id(VOICE)));

    apply(
        &mut store,
        "GUILD_DELETE",
        &json!({"id": FIRST.to_string()}).to_string(),
    );
    navigation.reconcile(&store);
    assert_eq!(navigation.selected_guild(), Some(id(SECOND)));
    assert!(navigation.remembered.is_empty());
    assert!(!navigation.select_guild(&store, id(FIRST)));
}

#[test]
fn a_different_account_never_inherits_channel_selection_or_memory() {
    let (mut store, mut navigation) = setup(READY);
    assert!(navigation.select_channel(&store, id(GENERAL)));
    let mut different = ready(READY);
    different.user.id = id(ME + 1);
    replace(&mut store, different);
    assert!(navigation.selection(&store).is_none());
    assert!(!navigation.select_channel(&store, id(GENERAL)));
    assert!(navigation.subscription_target(&store).guilds().is_empty());
    assert!(
        navigation
            .snapshot(&store, 0, 10, 0, 10)
            .selected_guild
            .is_none()
    );
    assert_eq!(navigation.channel_window(&store, 0, 10).total, 0);
    assert!(
        navigation
            .guild_window(&store, 0, 10)
            .rows
            .iter()
            .all(|row| !row.selected)
    );
    navigation.reconcile(&store);
    assert!(navigation.selected_channel().is_none());
    assert!(navigation.remembered.is_empty());
}

fn large_ready() -> Ready {
    let user = User {
        id: id(1),
        username: "large_fixture".to_owned(),
        global_name: None,
        avatar: None,
        bot: false,
    };
    let name = "界".repeat(160);
    Ready {
        session_id: SessionId::new("large-navigation-fixture".to_owned()),
        user,
        users: Vec::new(),
        guilds: (0..160)
            .map(|index| {
                let guild_id = id(100 + index);
                Guild {
                    id: guild_id,
                    name: name.clone(),
                    icon: None,
                    owner_id: Some(id(2)),
                    roles: vec![Role {
                        id: guild_id,
                        name: "@everyone".to_owned(),
                        permissions: Permissions::VIEW_CHANNEL
                            | Permissions::READ_MESSAGE_HISTORY
                            | Permissions::SEND_MESSAGES
                            | Permissions::CONNECT
                            | Permissions::SPEAK,
                        position: 0,
                    }],
                    channels: (0..if index == 0 { 3000 } else { 20 })
                        .map(|channel| Channel {
                            id: id(1_000_000 + index * 10_000 + channel),
                            guild_id: Some(guild_id),
                            kind: ModelChannelKind::GuildText,
                            name: Some(name.clone()),
                            position: Some(channel as i32),
                            parent_id: None,
                            permission_overwrites: Vec::new(),
                            recipients: Vec::new(),
                            recipient_ids: Vec::new(),
                            last_message_id: None,
                        })
                        .collect(),
                    members: vec![GuildMember {
                        user_id: id(1),
                        nick: None,
                        roles: Vec::new(),
                        roles_known: true,
                        communication_disabled_until: None,
                    }],
                    member_count: 1,
                }
            })
            .collect(),
        unavailable_guilds: Vec::new(),
        private_channels: Vec::new(),
    }
}

#[test]
fn large_store_windows_names_offsets_and_remembered_selections_are_bounded() {
    let mut store = Store::new();
    replace(&mut store, large_ready());
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let snapshot = navigation.snapshot(&store, 0, usize::MAX, 1_000, usize::MAX);
    assert_eq!(snapshot.guilds.total, 160);
    assert_eq!(snapshot.guilds.rows.len(), MAX_WINDOW_ROWS);
    assert_eq!(snapshot.channels.total, 3_000);
    assert_eq!(snapshot.channels.offset, 1_000);
    assert_eq!(snapshot.channels.rows.len(), MAX_WINDOW_ROWS);
    assert!(
        snapshot
            .guilds
            .rows
            .iter()
            .all(|row| row.name.len() <= MAX_NAME_BYTES)
    );
    assert!(
        snapshot
            .channels
            .rows
            .iter()
            .all(|row| row.name.len() == 255)
    );
    assert_eq!(snapshot.channels.rows[0].id, id(1_001_000));
    assert_eq!(
        navigation
            .guild_window(&store, usize::MAX, usize::MAX)
            .offset,
        160
    );
    assert_eq!(
        navigation
            .channel_window(&store, usize::MAX, usize::MAX)
            .offset,
        3_000
    );
    assert!(
        navigation
            .channel_window(&store, usize::MAX, usize::MAX)
            .rows
            .is_empty()
    );
    let empty = navigation.channel_window(&store, 123, 0);
    assert_eq!(
        (empty.offset, empty.total, empty.rows.len()),
        (123, 3_000, 0)
    );

    assert!(navigation.select_channel(&store, id(1_002_999)));
    assert_eq!(navigation.selection(&store).unwrap().name.len(), 255);
    let owned = snapshot.channels.rows[0].name.clone();
    let mut renamed = store
        .guild(id(100))
        .unwrap()
        .channel(id(1_001_000))
        .unwrap()
        .clone();
    renamed.name = Some("changed after snapshot".to_owned());
    update_channel(&mut store, renamed);
    assert_eq!(snapshot.channels.rows[0].name, owned);

    for index in 0..160 {
        assert!(navigation.select_guild(&store, id(100 + index)));
        assert!(navigation.select_channel(&store, id(1_000_000 + index * 10_000)));
        assert!(navigation.remembered.len() <= store.guild_ids().len());
    }
    assert_eq!(navigation.remembered.len(), 160);
    for index in 0..160 {
        store.apply(event(Dispatch::GuildDelete(GuildDelete {
            id: id(100 + index),
            unavailable: false,
        })));
        navigation.reconcile(&store);
        assert!(navigation.remembered.len() <= store.guild_ids().len());
    }
    assert!(navigation.remembered.is_empty());
    assert!(navigation.selected_guild().is_none());
    assert_eq!(
        navigation.snapshot(&store, 0, 128, 0, 128),
        NavigationSnapshot::default()
    );
    assert_eq!(store.bytes(), store.recount());
}

#[test]
fn timeout_expiry_is_rfc3339_and_evaluated_with_an_injected_instant() {
    let now = OffsetDateTime::parse("2026-10-07T12:00:00Z", &Rfc3339).unwrap();
    assert!(timeout_active("2026-10-07T12:00:00.001Z", now));
    assert!(!timeout_active("2026-10-07T12:00:00Z", now));
    assert!(!timeout_active("2026-10-07T13:59:59+02:00", now));
    assert!(timeout_active("2026-10-07T14:00:01+02:00", now));
    assert!(!timeout_active("2000-01-01T00:00:00.000000+00:00", now));
    assert!(
        timeout_active("not a timestamp", now),
        "unknown expiry cannot grant access"
    );
}

#[test]
fn timeout_permissions_use_injected_clock_without_polling_or_expired_restrictions() {
    let (mut store, _) = setup(NAV_READY);
    let now = OffsetDateTime::parse("2026-10-07T12:00:00Z", &Rfc3339).unwrap();
    for (until, blocked) in [
        ("2026-10-07T12:00:01Z", true),
        ("2026-10-07T11:59:59Z", false),
        ("invalid expiry", true),
    ] {
        apply(
            &mut store,
            "GUILD_MEMBER_UPDATE",
            &json!({
                "guild_id": "100", "user": {"id": "1"}, "communication_disabled_until": until
            })
            .to_string(),
        );
        let guild = store.guild(id(100)).unwrap();
        let context = PermissionContext::with_clock(&store, guild, || now).unwrap();
        let voice = context.permissions(guild.channel(id(1001)).unwrap());
        let text = context.permissions(guild.channel(id(1000)).unwrap());
        assert!(voice.view_channel && text.view_channel && text.read_history);
        assert_eq!(voice.connect, !blocked);
        assert_eq!(voice.speak, !blocked);
        assert_eq!(text.send_messages, !blocked);
    }
}
#[test]
fn private_conversations_are_listed_by_latest_activity_and_open_only_known_ids() {
    let (mut store, mut navigation) = setup(READY);
    let dm = id(300_000_000_000_000_001);
    let group = id(300_000_000_000_000_002);
    assert!(!navigation.select_private_channel(&store, id(999_999)));
    assert!(navigation.select_private_channel(&store, dm));
    navigation.reconcile(&store);
    let snapshot = navigation.snapshot(&store, 0, 20, 0, 20);
    assert_eq!(snapshot.private_selection.as_ref().unwrap().channel_id, dm);
    assert_eq!(snapshot.private_selection.as_ref().unwrap().name, "Nelly");
    assert_eq!(snapshot.private_channels.rows[0].id, dm);
    assert_eq!(snapshot.private_channels.rows[0].label, "Nelly");
    assert_eq!(snapshot.private_channels.rows[1].id, group);
    assert_eq!(snapshot.private_channels.rows[1].label, "Study group");
    assert!(snapshot.selected_guild.is_none());

    assert!(navigation.select_guild(&store, id(FIRST)));
    assert!(navigation.private_selection(&store).is_none());
    assert!(navigation.select_private_channel(&store, group));
    apply(
        &mut store,
        "CHANNEL_DELETE",
        r#"{"id":"300000000000000002","type":3}"#,
    );
    navigation.reconcile(&store);
    assert!(navigation.private_selection(&store).is_none());
}
