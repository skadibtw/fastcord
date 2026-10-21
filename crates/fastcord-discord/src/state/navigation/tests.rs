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
/// A forum channel in `ready.json` (a post list, not a text channel).
const FORUM: u64 = 41_771_983_423_143_950;

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
    assert_eq!(
        ids(&snapshot.channels.rows),
        [id(GENERAL), id(VOICE), id(FORUM)]
    );
    assert_eq!(snapshot.channels.rows[0].kind, ChannelKind::Text);
    assert_eq!(snapshot.channels.rows[1].kind, ChannelKind::Voice);
    assert_eq!(snapshot.channels.rows[2].kind, ChannelKind::Forum);
    assert!(snapshot.channels.rows.iter().all(|row| row.enabled));

    assert!(navigation.select_channel(&store, id(VOICE)));
    let selection = navigation.selection(&store).unwrap();
    assert_eq!(
        (selection.guild_id, selection.channel_id),
        (id(FIRST), id(VOICE))
    );
    assert!(selection.permissions.connect && selection.permissions.speak);
    assert!(selection.permissions.read_history && selection.permissions.send_messages);
    assert!(
        selection.permissions.add_reactions,
        "the everyone role of the fixture may react"
    );

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
            1000, 2502, 1001, 2201, 2301, 3000, 3002, 3003, 3004, 2000, 2002, 2003, 2001, 2100,
            2101, 2400,
        ]
        .map(id)
    );
    assert_eq!(before.len(), 16);
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
    assert!(
        !blocked.permissions.connect && blocked.enabled,
        "the chat of a voice channel opens without CONNECT"
    );
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
    for forbidden in [1002, 3001, 2200, 2000, 2501, 9999] {
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
    assert_eq!(
        ids(&rows(&navigation, &store)),
        [id(VOICE), id(GENERAL), id(FORUM)]
    );

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
            // Losing CONNECT keeps the chat but no longer allows joining.
            assert!(navigation.select_channel(&store, id(1001)));
            assert!(!navigation.selection(&store).unwrap().permissions.connect);
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
    assert!(navigation.select_channel(&store, id(3003)));
    assert!(!navigation.selection(&store).unwrap().permissions.connect);
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
    assert!(navigation.select_channel(&store, id(1001)));
    assert!(!navigation.selection(&store).unwrap().permissions.connect);
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
        navigation
            .selection(&store)
            .unwrap()
            .permissions
            .add_reactions,
        "the verified owner may react wherever overwrites deny it"
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
        assert!(navigation.select_channel(&store, id(1001)));
        assert!(
            !navigation.selection(&store).unwrap().permissions.connect,
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
        read: Default::default(),
        relationships: Vec::new(),
        ignored: Vec::new(),
        presences: Vec::new(),
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
                    emojis: Vec::new(),
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
                            owner_id: None,
                            icon: None,
                            last_message_id: None,
                            is_message_request: None,
                            is_spam: None,
                            thread_metadata: None,
                            member: None,
                            available_tags: Vec::new(),
                            applied_tags: Vec::new(),
                            flags: 0,
                            message_count: None,
                            default_sort_order: None,
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
                    stage_instances: Vec::new(),
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
        NavigationSnapshot {
            current_user: store.current_user().cloned(),
            ..NavigationSnapshot::default()
        }
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
    // @everyone may react, so the timeout is the only thing that can take it.
    apply(
        &mut store,
        "GUILD_ROLE_UPDATE",
        &json!({
            "guild_id": "100",
            "role": {"id": "100", "name": "@everyone", "permissions": (68_608_u64 | 64).to_string()}
        })
        .to_string(),
    );
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
        let voice = context.permissions(guild, guild.channel(id(1001)).unwrap());
        let text = context.permissions(guild, guild.channel(id(1000)).unwrap());
        assert!(voice.view_channel && text.view_channel && text.read_history);
        assert_eq!(voice.connect, !blocked);
        assert_eq!(voice.speak, !blocked);
        assert_eq!(text.send_messages, !blocked);
        assert_eq!(text.add_reactions, !blocked);
        assert_eq!(voice.add_reactions, !blocked);
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

#[test]
fn add_reactions_follows_the_role_and_overwrite_rules() {
    let (mut store, mut navigation) = setup(NAV_READY);
    let grant = |store: &mut Store, role: u64, permissions: u64| {
        apply(
            store,
            "GUILD_ROLE_UPDATE",
            &json!({
                "guild_id": "100",
                "role": {"id": role.to_string(), "name": "role", "permissions": permissions.to_string()}
            })
            .to_string(),
        );
    };
    let may_react = |navigation: &Navigation, store: &Store| {
        navigation
            .selection(store)
            .unwrap()
            .permissions
            .add_reactions
    };
    assert!(navigation.select_channel(&store, id(1000)));
    // The fixture's @everyone (view, send, read history) cannot react.
    let summary = navigation.selection(&store).unwrap().permissions;
    assert!(summary.view_channel && summary.read_history && summary.send_messages);
    assert!(!summary.add_reactions);

    // A role the member holds grants it, and losing the permission loses it.
    grant(&mut store, 102, 64);
    assert!(may_react(&navigation, &store));
    grant(&mut store, 102, 0);
    assert!(!may_react(&navigation, &store));
    // @everyone grants it to everyone.
    grant(&mut store, 100, 68_608 | 64);
    assert!(may_react(&navigation, &store));

    // A member overwrite that denies it wins over the role, and only it.
    let mut channel = store
        .guild(id(100))
        .unwrap()
        .channel(id(1000))
        .unwrap()
        .clone();
    channel.permission_overwrites.push(PermissionOverwrite {
        id: id(1),
        kind: fastcord_model::OverwriteKind::Member,
        allow: Permissions::NONE,
        deny: Permissions::ADD_REACTIONS,
    });
    update_channel(&mut store, channel.clone());
    let summary = navigation.selection(&store).unwrap().permissions;
    assert!(!summary.add_reactions);
    assert!(summary.view_channel && summary.read_history && summary.send_messages);

    // An allow overwrite grants it where no role does.
    grant(&mut store, 100, 68_608);
    channel.permission_overwrites = vec![PermissionOverwrite {
        id: id(1),
        kind: fastcord_model::OverwriteKind::Member,
        allow: Permissions::ADD_REACTIONS,
        deny: Permissions::NONE,
    }];
    update_channel(&mut store, channel.clone());
    assert!(may_react(&navigation, &store));
    // Without it, nobody else's overwrite matters.
    channel.permission_overwrites = vec![PermissionOverwrite {
        id: id(2),
        kind: fastcord_model::OverwriteKind::Member,
        allow: Permissions::ADD_REACTIONS,
        deny: Permissions::NONE,
    }];
    update_channel(&mut store, channel);
    assert!(!may_react(&navigation, &store));

    // The rows of the channel list carry the same summary.
    let rows = rows(&navigation, &store);
    let row = rows.iter().find(|row| row.id == id(1000)).unwrap();
    assert!(!row.permissions.add_reactions);
    assert!(row.permissions.view_channel);
}

// ----- threads (milestone 48); every payload here is synthetic and unverified -----

const THREAD_SEND: Permissions = Permissions::SEND_MESSAGES_IN_THREADS;

fn thread_json(id: u64, kind: u8, parent: u64, extra: Value) -> Value {
    let mut value = json!({
        "id": id.to_string(), "guild_id": "100", "type": kind, "name": format!("t{id}"),
        "parent_id": parent.to_string(),
        "thread_metadata": {"archived": false, "locked": false},
    });
    for (key, field) in extra.as_object().unwrap() {
        value[key] = field.clone();
    }
    value
}

/// Guild 100 owned by 2; the account (1) holds role 200 with `permissions`.
/// Channel 10 is open text, channel 11 hides itself from the account.
fn thread_store(permissions: Permissions, threads: &[Value]) -> (Store, Navigation) {
    let raw = json!({
        "id": "100", "name": "G", "owner_id": "2", "member_count": 2,
        "roles": [
            {"id": "100", "name": "@everyone", "permissions": (Permissions::VIEW_CHANNEL
                | Permissions::READ_MESSAGE_HISTORY).0.to_string()},
            {"id": "200", "name": "r", "permissions": permissions.0.to_string()},
        ],
        "channels": [
            {"id": "10", "type": 0, "name": "general"},
            {"id": "11", "type": 0, "name": "secret", "permission_overwrites":
                [{"id": "100", "type": 0, "allow": "0", "deny": "1024"}]},
        ],
        "threads": threads,
        "members": [{"user": {"id": "1", "username": "me"}, "roles": ["200"]}],
    });
    let mut store = Store::new();
    let mut start = ready(NAV_READY);
    start.guilds.clear();
    start.unavailable_guilds.clear();
    replace(&mut store, start);
    apply(&mut store, "GUILD_CREATE", &raw.to_string());
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    assert!(navigation.select_guild(&store, id(100)));
    (store, navigation)
}

fn thread_rows(navigation: &Navigation, store: &Store) -> Vec<(u64, bool)> {
    rows(navigation, store)
        .iter()
        .map(|row| (row.id.0, row.thread.is_some()))
        .collect()
}

const ACCESS: Permissions =
    Permissions(Permissions::VIEW_CHANNEL.0 | Permissions::READ_MESSAGE_HISTORY.0 | THREAD_SEND.0);

#[test]
fn threads_list_under_their_parent_and_only_when_accessible() {
    let (store, navigation) = thread_store(
        ACCESS,
        &[
            thread_json(30, 11, 10, json!({})),
            thread_json(20, 11, 10, json!({})),
            // archived: reached through the archive list
            thread_json(21, 11, 10, json!({"thread_metadata": {"archived": true}})),
            // private and not joined: never exposed
            thread_json(22, 12, 10, json!({})),
            thread_json(23, 12, 10, json!({"member": {"user_id": "1"}})),
            // parent the account cannot view, and a parent that is not held
            thread_json(24, 11, 11, json!({})),
            thread_json(25, 11, 99, json!({})),
        ],
    );
    assert_eq!(
        thread_rows(&navigation, &store),
        [(10, false), (20, true), (23, true), (30, true)]
    );
    let rows = rows(&navigation, &store);
    assert_eq!(rows[1].kind, ChannelKind::Thread);
    assert!(rows[1].enabled && rows[1].permissions.send_messages);
    let state = rows[2].thread.unwrap();
    assert!(state.joined && !state.archived && state.parent_id == id(10));
    assert_eq!(store.bytes(), store.recount());
}

#[test]
fn an_archived_thread_can_be_opened_and_shows_while_open() {
    let (store, mut navigation) = thread_store(
        ACCESS,
        &[thread_json(
            21,
            11,
            10,
            json!({"thread_metadata": {"archived": true, "locked": false}}),
        )],
    );
    assert!(navigation.select_channel(&store, id(21)));
    let selection = navigation.selection(&store).unwrap();
    assert_eq!(selection.kind, ChannelKind::Thread);
    assert!(selection.thread.unwrap().archived);
    assert!(selection.permissions.send_messages, "writing unarchives it");
    assert_eq!(thread_rows(&navigation, &store), [(10, false), (21, true)]);
    // The hidden parent and a private thread stay closed to explicit selection.
    let (store, mut navigation) = thread_store(
        ACCESS,
        &[
            thread_json(22, 12, 10, json!({})),
            thread_json(24, 11, 11, json!({})),
        ],
    );
    assert!(!navigation.select_channel(&store, id(22)));
    assert!(!navigation.select_channel(&store, id(24)));
}

#[test]
fn locked_threads_are_read_only_without_manage_threads() {
    let locked = thread_json(20, 11, 10, json!({"thread_metadata": {"locked": true}}));
    let (store, navigation) = thread_store(ACCESS, std::slice::from_ref(&locked));
    let row = &rows(&navigation, &store)[1];
    assert!(row.thread.unwrap().locked);
    assert!(row.permissions.view_channel && row.permissions.read_history);
    assert!(!row.permissions.send_messages);

    let (store, navigation) = thread_store(ACCESS | Permissions::MANAGE_THREADS, &[locked]);
    assert!(rows(&navigation, &store)[1].permissions.send_messages);

    // Plain SEND_MESSAGES does not write in a thread.
    let (store, navigation) = thread_store(
        Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY | Permissions::SEND_MESSAGES,
        &[thread_json(20, 11, 10, json!({}))],
    );
    assert!(!rows(&navigation, &store)[1].permissions.send_messages);
}

#[test]
fn thread_managers_see_private_threads_and_create_rights_are_reported() {
    let (store, navigation) = thread_store(
        ACCESS | Permissions::MANAGE_THREADS | Permissions::CREATE_PUBLIC_THREADS,
        &[thread_json(22, 12, 10, json!({}))],
    );
    assert_eq!(thread_rows(&navigation, &store), [(10, false), (22, true)]);
    let rows = rows(&navigation, &store);
    assert!(rows[0].permissions.create_threads && rows[0].permissions.manage_threads);
}

#[test]
fn thread_events_converge_and_the_ledger_never_drifts() {
    let (mut store, mut navigation) = thread_store(ACCESS, &[thread_json(20, 11, 10, json!({}))]);
    // THREAD_CREATE (ours: it carries a member object) and THREAD_UPDATE keeping membership.
    apply(
        &mut store,
        "THREAD_CREATE",
        &thread_json(31, 11, 10, json!({"member": {"user_id": "1"}})).to_string(),
    );
    apply(
        &mut store,
        "THREAD_UPDATE",
        &thread_json(31, 11, 10, json!({"name": "renamed"})).to_string(),
    );
    let rows_now = rows(&navigation, &store);
    let created = rows_now.iter().find(|r| r.id == id(31)).unwrap();
    assert_eq!(created.name, "renamed");
    assert!(created.thread.unwrap().joined, "silence is not leaving");

    // Archiving removes it from the sidebar; deleting forgets it.
    apply(
        &mut store,
        "THREAD_UPDATE",
        &thread_json(20, 11, 10, json!({"thread_metadata": {"archived": true}})).to_string(),
    );
    assert_eq!(ids(&rows(&navigation, &store)), [id(10), id(31)]);
    apply(
        &mut store,
        "THREAD_DELETE",
        r#"{"id":"31","guild_id":"100","parent_id":"10","type":11}"#,
    );
    assert_eq!(ids(&rows(&navigation, &store)), [id(10)]);

    // A private thread appears when the account is added, and goes when it leaves.
    apply(
        &mut store,
        "THREAD_CREATE",
        &thread_json(22, 12, 10, json!({})).to_string(),
    );
    assert_eq!(ids(&rows(&navigation, &store)), [id(10)]);
    apply(
        &mut store,
        "THREAD_MEMBER_UPDATE",
        r#"{"id":"22","guild_id":"100","user_id":"1","flags":0}"#,
    );
    assert_eq!(ids(&rows(&navigation, &store)), [id(10), id(22)]);
    assert!(navigation.select_channel(&store, id(22)));
    apply(
        &mut store,
        "THREAD_MEMBERS_UPDATE",
        r#"{"id":"22","guild_id":"100","member_count":1,"removed_member_ids":["77"]}"#,
    );
    assert_eq!(
        ids(&rows(&navigation, &store)),
        [id(10), id(22)],
        "someone else left"
    );
    apply(
        &mut store,
        "THREAD_MEMBERS_UPDATE",
        r#"{"id":"22","guild_id":"100","member_count":0,"removed_member_ids":["1"]}"#,
    );
    assert_eq!(ids(&rows(&navigation, &store)), [id(10)]);
    navigation.reconcile(&store);
    assert!(
        navigation.selection(&store).is_none(),
        "leaving closes a private thread"
    );

    // Deleting the parent takes its threads along.
    apply(
        &mut store,
        "CHANNEL_DELETE",
        r#"{"id":"10","guild_id":"100","type":0}"#,
    );
    assert!(
        store
            .guild(id(100))
            .unwrap()
            .channels()
            .all(|c| !c.kind.is_thread())
    );
}

#[test]
fn thread_list_sync_replaces_only_the_listed_parents() {
    let (mut store, navigation) = thread_store(
        ACCESS,
        &[
            thread_json(20, 11, 10, json!({})),
            thread_json(26, 11, 11, json!({})),
        ],
    );
    let sync = |channels: Value, threads: Vec<Value>, members: Value| {
        json!({"guild_id": "100", "channel_ids": channels, "threads": threads,
               "members": members})
        .to_string()
    };
    apply(
        &mut store,
        "THREAD_LIST_SYNC",
        &sync(
            json!(["10"]),
            vec![thread_json(40, 12, 10, json!({}))],
            json!([{"id": "40", "user_id": "1"}]),
        ),
    );
    let held: Vec<_> = store
        .guild(id(100))
        .unwrap()
        .channels()
        .filter(|c| c.kind.is_thread())
        .map(|c| (c.id.0, c.member.is_some()))
        .collect();
    assert_eq!(
        held,
        [(26, false), (40, true)],
        "20 is gone, 26 under 11 stays"
    );
    assert_eq!(ids(&rows(&navigation, &store)), [id(10), id(40)]);
    // An unscoped sync replaces every thread of the guild.
    apply(
        &mut store,
        "THREAD_LIST_SYNC",
        &sync(json!([]), vec![], json!([])),
    );
    assert!(
        store
            .guild(id(100))
            .unwrap()
            .channels()
            .all(|c| !c.kind.is_thread())
    );
    // An unknown guild changes nothing.
    let unknown = sync(json!([]), vec![], json!([])).replace("\"100\"", "\"555\"");
    assert!(
        apply(&mut store, "THREAD_LIST_SYNC", &unknown)
            .guilds
            .is_empty()
    );
}

#[test]
fn threads_are_capped_per_guild_dropping_unjoined_and_archived_first() {
    let many: Vec<Value> = (0..crate::state::MAX_THREADS_PER_GUILD as u64 + 5)
        .map(|n| {
            let extra = match n {
                0 => json!({"member": {"user_id": "1"}}),
                1 => json!({"thread_metadata": {"archived": true}}),
                _ => json!({}),
            };
            thread_json(1_000 + n, 11, 10, extra)
        })
        .collect();
    let (mut store, _) = thread_store(ACCESS, &many);
    let threads = |store: &Store| {
        store
            .guild(id(100))
            .unwrap()
            .channels()
            .filter(|c| c.kind.is_thread())
            .map(|c| c.id.0)
            .collect::<Vec<_>>()
    };
    let held = threads(&store);
    assert_eq!(held.len(), crate::state::MAX_THREADS_PER_GUILD);
    assert!(held.contains(&1_000), "joined threads outlive the cap");
    assert!(!held.contains(&1_001), "archived ones go first");
    // One more arrives: an old unjoined thread makes room, and the ledger agrees.
    apply(
        &mut store,
        "THREAD_CREATE",
        &thread_json(9_999, 11, 10, json!({})).to_string(),
    );
    let held = threads(&store);
    assert_eq!(held.len(), crate::state::MAX_THREADS_PER_GUILD);
    assert!(held.contains(&9_999) && held.contains(&1_000));
}

mod inbox {
    use super::*;
    use crate::gateway::ReadStateEntry;

    const OTHER: u64 = 9_999;
    const NAV_GUILD: u64 = 100;

    fn cursor(channel: u64, acked: u64, mentions: u32) -> ReadStateEntry {
        ReadStateEntry {
            channel_id: id(channel),
            last_acked: id(acked),
            mention_count: mentions,
        }
    }

    fn with_cursors(entries: Vec<ReadStateEntry>) -> (Store, Navigation) {
        let mut store = Store::new();
        let mut ready = ready(NAV_READY);
        ready.read.entries = entries;
        replace(&mut store, ready);
        let mut navigation = Navigation::default();
        navigation.reconcile(&store);
        (store, navigation)
    }

    fn create(store: &mut Store, channel: u64, message: u64) {
        let raw = json!({
            "id": message.to_string(), "channel_id": channel.to_string(),
            "guild_id": NAV_GUILD.to_string(),
            "author": {"id": OTHER.to_string(), "username": "other"},
            "content": "x", "timestamp": "2026-10-16T00:00:00.000000+00:00",
        });
        apply(store, "MESSAGE_CREATE", &raw.to_string());
    }

    #[test]
    fn it_lists_what_the_badges_flag_mentions_first_and_agrees_with_them() {
        let (mut store, navigation) = with_cursors(vec![cursor(1000, 5, 2), cursor(2002, 5, 0)]);
        create(&mut store, 2002, 50);
        let inbox = navigation.inbox(&store, 10);
        assert_eq!(inbox.total, 2);
        let ids: Vec<_> = inbox.rows.iter().map(|row| row.channel).collect();
        assert_eq!(ids, [id(1000), id(2002)], "mentions first");
        assert_eq!(inbox.rows[0].mentions, 2);
        assert!(!inbox.rows[0].unread, "no newer message than the cursor");
        assert!(inbox.rows[1].unread);
        assert_eq!(inbox.rows[1].guild, Some(id(NAV_GUILD)));
        // The channel badges say the same.
        for row in navigation.channel_window(&store, 0, usize::MAX).rows {
            let listed = inbox.rows.iter().find(|r| r.channel == row.id);
            assert_eq!(
                listed.is_some(),
                row.unread || row.mentions > 0,
                "{}",
                row.id
            );
        }
        // Acknowledging empties the row on the next query.
        store.ack_local(id(2002), id(50));
        assert_eq!(navigation.inbox(&store, 10).total, 1);
    }

    #[test]
    fn mutes_show_and_the_cap_and_zero_limit_hold() {
        let (mut store, navigation) = with_cursors(vec![cursor(1000, 5, 1)]);
        store.read.update_mutes(None, false, &[(id(1000), true)]);
        let inbox = navigation.inbox(&store, 1);
        assert!(inbox.rows[0].muted && inbox.rows[0].channel_muted);
        assert_eq!(navigation.inbox(&store, 0), Inbox::default());
        assert!(navigation.inbox(&store, usize::MAX).rows.len() <= MAX_INBOX_ROWS);
    }

    #[test]
    fn opening_by_channel_id_selects_its_guild_and_refuses_unknown_channels() {
        let (store, mut navigation) = with_cursors(Vec::new());
        assert!(navigation.open_channel(&store, id(1000)));
        assert_eq!(navigation.selected_guild(), Some(id(NAV_GUILD)));
        assert!(!navigation.open_channel(&store, id(1)));
        assert!(
            navigation.open_channel(&store, id(3003)),
            "voice chat without connect"
        );
    }
}

#[test]
fn group_membership_owner_name_and_icon_converge_and_replays_are_harmless() {
    let (mut store, mut navigation) = setup(READY);
    let group = id(300_000_000_000_000_002);
    assert!(navigation.select_private_channel(&store, group));
    let me = store.current_user().unwrap().id;
    let selection = |store: &Store, navigation: &Navigation| {
        navigation
            .snapshot(store, 0, 20, 0, 20)
            .private_selection
            .unwrap()
    };
    let before = selection(&store, &navigation);
    assert!(before.group);

    // Owner, name and icon arrive with CHANNEL_UPDATE; a partial update that
    // omits the participants keeps them.
    let members = before.members.clone();
    let changes = apply(
        &mut store,
        "CHANNEL_UPDATE",
        r#"{"id":"300000000000000002","type":3,"name":"Team","owner_id":"1234","icon":"abc"}"#,
    );
    assert!(changes.private_channels);
    let after = selection(&store, &navigation);
    assert_eq!(after.members, members);
    assert_eq!(after.custom_name, "Team");
    assert_eq!(after.owner, Some(id(1234)));
    assert!(after.has_icon);
    // An update without an owner keeps the known one (the icon hash is exact).
    apply(
        &mut store,
        "CHANNEL_UPDATE",
        r#"{"id":"300000000000000002","type":3,"name":"Team"}"#,
    );
    let kept = selection(&store, &navigation);
    assert_eq!(kept.owner, Some(id(1234)));
    assert!(!kept.has_icon);

    // Add, replayed add, add of the account itself, then removal and its replay.
    let add = r#"{"channel_id":"300000000000000002","user":{"id":"777","username":"Newcomer"}}"#;
    assert!(apply(&mut store, "CHANNEL_RECIPIENT_ADD", add).private_channels);
    assert!(!apply(&mut store, "CHANNEL_RECIPIENT_ADD", add).private_channels);
    let myself =
        format!(r#"{{"channel_id":"300000000000000002","user":{{"id":"{me}","username":"me"}}}}"#);
    assert!(!apply(&mut store, "CHANNEL_RECIPIENT_ADD", &myself).private_channels);
    assert!(
        selection(&store, &navigation)
            .members
            .iter()
            .any(|m| m.id == id(777))
    );
    let remove = r#"{"channel_id":"300000000000000002","user":{"id":"777"}}"#;
    assert!(apply(&mut store, "CHANNEL_RECIPIENT_REMOVE", remove).private_channels);
    assert!(!apply(&mut store, "CHANNEL_RECIPIENT_REMOVE", remove).private_channels);
    assert_eq!(selection(&store, &navigation).members, members);

    // Events for conversations the account does not hold change nothing.
    let stray = r#"{"channel_id":"999","user":{"id":"5","username":"x"}}"#;
    assert!(!apply(&mut store, "CHANNEL_RECIPIENT_ADD", stray).private_channels);

    // Being removed closes the conversation.
    let leave = format!(r#"{{"channel_id":"300000000000000002","user":{{"id":"{me}"}}}}"#);
    apply(&mut store, "CHANNEL_RECIPIENT_REMOVE", &leave);
    navigation.reconcile(&store);
    assert!(navigation.private_selection(&store).is_none());
}

#[test]
fn own_nick_members_and_roles_follow_the_selected_guild_and_member_events() {
    let (mut store, mut navigation) = setup(NAV_READY);
    assert!(navigation.select_guild(&store, id(100)));
    let snapshot = navigation.snapshot(&store, 0, 128, 0, 128);
    assert_eq!(snapshot.own_nick, None);
    // @everyone is not listed; the rest are highest position first.
    assert_eq!(
        snapshot.guild_roles,
        [
            (id(102), "Allowed".to_owned()),
            (id(101), "Voice".to_owned())
        ]
    );
    assert_eq!(snapshot.members[&id(1)].roles, [id(101), id(102)]);
    assert_eq!(snapshot.members[&id(1)].nick, None);

    let set = r#"{"guild_id":"100","user":{"id":"1"},"nick":"Nell"}"#;
    apply(&mut store, "GUILD_MEMBER_UPDATE", set);
    apply(&mut store, "GUILD_MEMBER_UPDATE", set); // replay
    let snapshot = navigation.snapshot(&store, 0, 128, 0, 128);
    assert_eq!(snapshot.own_nick.as_deref(), Some("Nell"));
    assert_eq!(snapshot.members[&id(1)].nick.as_deref(), Some("Nell"));
    assert_eq!(snapshot.members[&id(1)].roles, [id(101), id(102)]);

    // An explicit null clears it; an empty nickname is no nickname.
    apply(
        &mut store,
        "GUILD_MEMBER_UPDATE",
        r#"{"guild_id":"100","user":{"id":"1"},"nick":null}"#,
    );
    let snapshot = navigation.snapshot(&store, 0, 128, 0, 128);
    assert_eq!(snapshot.own_nick, None);
    assert_eq!(snapshot.members[&id(1)].nick, None);
}

/// Text chat in voice and Stage channels: the same channel ID and message
/// routes as a text channel, gated on VIEW_CHANNEL (CONNECT only joins the call).
mod voice_chat {
    use super::*;
    use crate::gateway::ReadStateEntry;
    use fastcord_model::OverwriteKind;

    const VOICE_CHANNEL: u64 = 1001;
    const NO_CONNECT: u64 = 3003;
    const STAGE: u64 = 3004;

    fn with_cursors(entries: Vec<ReadStateEntry>) -> (Store, Navigation) {
        let mut store = Store::new();
        let mut ready = ready(NAV_READY);
        ready.read.entries = entries;
        replace(&mut store, ready);
        let mut navigation = Navigation::default();
        navigation.reconcile(&store);
        (store, navigation)
    }

    fn create(store: &mut Store, channel: u64, message: u64, mention_me: bool) {
        let mentions = if mention_me {
            json!([{"id": "1", "username": "me"}])
        } else {
            json!([])
        };
        let raw = json!({
            "id": message.to_string(), "channel_id": channel.to_string(),
            "guild_id": "100", "mentions": mentions,
            "author": {"id": "9999", "username": "other"},
            "content": "x", "timestamp": "2026-10-16T00:00:00.000000+00:00",
        });
        apply(store, "MESSAGE_CREATE", &raw.to_string());
    }

    fn row(navigation: &Navigation, store: &Store, channel: u64) -> ChannelRow {
        rows(navigation, store)
            .into_iter()
            .find(|row| row.id == id(channel))
            .unwrap()
    }

    #[test]
    fn chat_opens_with_view_channel_alone_and_connect_only_gates_joining() {
        let (store, mut navigation) = setup(NAV_READY);
        let blocked = row(&navigation, &store, NO_CONNECT);
        assert!(blocked.enabled && !blocked.permissions.connect);
        for channel in [VOICE_CHANNEL, NO_CONNECT, STAGE] {
            assert!(navigation.select_channel(&store, id(channel)), "{channel}");
            let selection = navigation.selection(&store).unwrap();
            assert_eq!(selection.kind, ChannelKind::Voice);
            assert!(selection.permissions.read_history && selection.permissions.send_messages);
            let target = navigation.subscription_target(&store);
            assert_eq!(target.selected_channel(), Some(id(channel)));
        }
        // A channel the account cannot view stays closed.
        assert!(!navigation.select_channel(&store, id(1002)));
        assert_eq!(navigation.selected_channel(), Some(id(STAGE)));
    }

    #[test]
    fn overwrite_changes_update_sendability_without_closing_the_chat() {
        let (mut store, mut navigation) = setup(NAV_READY);
        assert!(navigation.select_channel(&store, id(VOICE_CHANNEL)));
        let mut channel = store
            .guild(id(100))
            .unwrap()
            .channel(id(VOICE_CHANNEL))
            .unwrap()
            .clone();
        let deny = |permissions| {
            vec![PermissionOverwrite {
                id: id(1),
                kind: OverwriteKind::Member,
                allow: Permissions::NONE,
                deny: permissions,
            }]
        };
        // SEND_MESSAGES denied: readable, not sendable, still open.
        channel.permission_overwrites = deny(Permissions::SEND_MESSAGES);
        update_channel(&mut store, channel.clone());
        navigation.reconcile(&store);
        let selection = navigation.selection(&store).unwrap();
        assert!(selection.permissions.read_history && !selection.permissions.send_messages);
        // READ_MESSAGE_HISTORY denied as well: open but unreadable.
        channel.permission_overwrites =
            deny(Permissions::SEND_MESSAGES | Permissions::READ_MESSAGE_HISTORY);
        update_channel(&mut store, channel.clone());
        assert!(
            !navigation
                .selection(&store)
                .unwrap()
                .permissions
                .read_history
        );
        // VIEW_CHANNEL denied: the chat closes.
        channel.permission_overwrites = deny(Permissions::VIEW_CHANNEL);
        update_channel(&mut store, channel.clone());
        navigation.reconcile(&store);
        assert!(navigation.selection(&store).is_none());
        // Restored: it can be reopened with full rights.
        channel.permission_overwrites = Vec::new();
        update_channel(&mut store, channel);
        assert!(navigation.select_channel(&store, id(VOICE_CHANNEL)));
        assert!(
            navigation
                .selection(&store)
                .unwrap()
                .permissions
                .send_messages
        );
    }

    #[test]
    fn messages_mark_the_voice_row_unread_and_count_mentions() {
        let cursor = |channel, mentions| ReadStateEntry {
            channel_id: id(channel),
            last_acked: id(5),
            mention_count: mentions,
        };
        let (mut store, navigation) =
            with_cursors(vec![cursor(VOICE_CHANNEL, 0), cursor(STAGE, 0)]);
        assert!(!row(&navigation, &store, VOICE_CHANNEL).unread);
        create(&mut store, VOICE_CHANNEL, 50, false);
        let voice = row(&navigation, &store, VOICE_CHANNEL);
        assert!(voice.unread && voice.mentions == 0);
        // A replay of the same event changes nothing.
        create(&mut store, VOICE_CHANNEL, 50, false);
        assert_eq!(row(&navigation, &store, VOICE_CHANNEL), voice);
        create(&mut store, STAGE, 51, true);
        let stage = row(&navigation, &store, STAGE);
        assert!(stage.unread && stage.mentions == 1);
        assert!(!row(&navigation, &store, NO_CONNECT).unread);
        // The inbox lists voice chats like other conversations.
        let listed: Vec<_> = navigation
            .inbox(&store, 10)
            .rows
            .iter()
            .map(|row| row.channel)
            .collect();
        assert_eq!(listed, [id(STAGE), id(VOICE_CHANNEL)]);
    }
}

#[test]
fn voice_rows_are_followed_by_their_capped_occupants() {
    let (mut store, navigation) = setup(READY);
    for user in 1..=11u64 {
        let state = fastcord_model::VoiceState {
            guild_id: Some(id(FIRST)),
            channel_id: Some(id(VOICE)),
            user_id: id(user),
            session_id: "v".to_owned(),
            deaf: false,
            mute: user == 1,
            self_deaf: user == 2,
            self_mute: false,
            self_stream: user == 3,
            self_video: user == 4,
            suppress: false,
            request_to_speak_timestamp: None,
        };
        store.apply(event(Dispatch::VoiceStateUpdate(Box::new(
            crate::gateway::VoiceStateUpdate {
                state,
                member: None,
                users: vec![User {
                    id: id(user),
                    username: format!("user{user}"),
                    global_name: None,
                    avatar: None,
                    bot: false,
                }],
            },
        ))));
    }
    let window = navigation.channel_window(&store, 0, usize::MAX);
    // text + voice + forum + 8 people + "+3"
    assert_eq!(window.total, 3 + 8 + 1);
    assert_eq!(window.rows.len(), window.total);
    assert!(window.rows[1].occupant.is_none());
    assert!(matches!(
        &window.rows[2].occupant,
        Some(Occupant::Member { user_id, name, muted: true, deafened: false, .. })
            if *user_id == id(1) && name == "user1"
    ));
    assert!(matches!(
        &window.rows[3].occupant,
        Some(Occupant::Member { deafened: true, .. })
    ));
    assert!(matches!(
        &window.rows[4].occupant,
        Some(Occupant::Member {
            streaming: true,
            video: false,
            ..
        })
    ));
    assert!(matches!(
        &window.rows[5].occupant,
        Some(Occupant::Member {
            streaming: false,
            video: true,
            ..
        })
    ));
    assert_eq!(window.rows[10].occupant, Some(Occupant::More(3)));
    // A window in the middle of the list slices the same flat rows.
    let part = navigation.channel_window(&store, 9, 2);
    assert_eq!((part.offset, part.total, part.rows.len()), (9, 12, 2));
    assert_eq!(part.rows[1].occupant, Some(Occupant::More(3)));
}

fn quick_names(navigation: &Navigation, store: &Store, query: &str) -> Vec<String> {
    navigation
        .quick_switch(store, query, MAX_QUICK_RESULTS)
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

#[test]
fn quick_switch_matches_prefix_then_substring_then_subsequence_and_marks_kinds() {
    let (store, navigation) = setup(READY);
    // Everything openable, the forum (a post list since M49) included.
    let all = quick_names(&navigation, &store, "");
    assert_eq!(all.len(), 6, "{all:?}");
    assert!(all.iter().any(|n| n.contains("forum")));
    assert_eq!(quick_names(&navigation, &store, "gen"), ["general"]);
    assert_eq!(quick_names(&navigation, &store, "#GEN"), ["general"]);
    assert_eq!(quick_names(&navigation, &store, "grl"), ["general"]);
    assert_eq!(
        quick_names(&navigation, &store, "nel")[0],
        "Nelly",
        "a prefix ranks first"
    );
    // The server name takes part as a weak match: "second lob" finds lobby.
    assert_eq!(quick_names(&navigation, &store, "second lob")[0], "lobby");
    assert!(quick_names(&navigation, &store, "zzz").is_empty());
    let voice = navigation.quick_switch(&store, "voice", 10);
    assert_eq!(voice[0].kind, QuickKind::Voice);
    assert_eq!(voice[0].guild, Some(id(FIRST)));
    let group = navigation.quick_switch(&store, "study", 10);
    assert_eq!((group[0].kind, group[0].guild), (QuickKind::Group, None));
    // A prefix beats a substring.
    let ranked = quick_names(&navigation, &store, "o");
    assert!(ranked[0] == "lobby" || ranked[0] == "Voice", "{ranked:?}");
}

#[test]
fn quick_switch_is_bounded_and_rejects_a_foreign_account() {
    let mut store = Store::new();
    replace(&mut store, large_ready());
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    let all = navigation.quick_switch(&store, "", usize::MAX);
    assert_eq!(all.len(), MAX_QUICK_RESULTS);
    assert!(all.iter().all(|entry| entry.name.len() <= MAX_NAME_BYTES));
    assert_eq!(navigation.quick_switch(&store, "", 3).len(), 3);
    assert!(
        Navigation::default()
            .quick_switch(&store, "", 10)
            .is_empty()
    );
}

#[test]
fn keyboard_steps_wrap_over_channels_unreads_and_servers() {
    let (store, mut navigation) = setup(READY);
    let step = |navigation: &mut Navigation, scope, forward| {
        assert!(navigation.step(&store, Step { scope, forward }));
        navigation.selected_channel()
    };
    // Nothing open: forward starts at the first, backward at the last.
    assert_eq!(
        step(&mut navigation, StepScope::Channel, true),
        Some(id(GENERAL))
    );
    assert_eq!(
        step(&mut navigation, StepScope::Channel, true),
        Some(id(VOICE))
    );
    // The forum row is a stop too, like in the official client.
    assert_eq!(
        step(&mut navigation, StepScope::Channel, true),
        Some(id(FORUM))
    );
    assert_eq!(
        step(&mut navigation, StepScope::Channel, true),
        Some(id(GENERAL))
    );
    assert_eq!(
        step(&mut navigation, StepScope::Channel, false),
        Some(id(FORUM))
    );
    assert_eq!(
        step(&mut navigation, StepScope::Channel, false),
        Some(id(VOICE))
    );
    // Servers, with each remembering its last channel.
    assert!(navigation.step(
        &store,
        Step {
            scope: StepScope::Server,
            forward: true
        }
    ));
    assert_eq!(navigation.selected_guild(), Some(id(SECOND)));
    assert_eq!(navigation.selected_channel(), None);
    assert_eq!(
        step(&mut navigation, StepScope::Channel, true),
        Some(id(LOBBY))
    );
    assert!(navigation.step(
        &store,
        Step {
            scope: StepScope::Server,
            forward: true
        }
    ));
    assert_eq!(navigation.selected_guild(), Some(id(FIRST)));
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));
    // Direct messages step through the DM list.
    assert!(navigation.select_private_channel(&store, id(300_000_000_000_000_001)));
    assert!(navigation.step(
        &store,
        Step {
            scope: StepScope::Channel,
            forward: true
        }
    ));
    assert!(navigation.private_selection(&store).is_some());
    assert_eq!(
        navigation.private_selection(&store).unwrap().channel_id,
        id(300_000_000_000_000_002)
    );
    // Leaving the DMs by server step.
    assert!(navigation.step(
        &store,
        Step {
            scope: StepScope::Server,
            forward: true
        }
    ));
    assert!(navigation.selected_guild().is_some());
}

#[test]
fn unread_steps_skip_read_conversations_and_report_none() {
    let mut store = Store::new();
    let mut r = ready(READY);
    r.read.entries = vec![crate::gateway::ReadStateEntry {
        channel_id: id(VOICE),
        last_acked: id(5),
        mention_count: 0,
    }];
    replace(&mut store, r);
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    assert!(navigation.select_channel(&store, id(GENERAL)));
    let unread = Step {
        scope: StepScope::Unread,
        forward: true,
    };
    assert!(!navigation.step(&store, unread), "nothing unread");
    assert_eq!(navigation.selected_channel(), Some(id(GENERAL)));
    let raw = json!({
        "id": "50", "channel_id": VOICE.to_string(), "guild_id": FIRST.to_string(),
        "author": {"id": "7", "username": "other"},
        "content": "x", "timestamp": "2026-10-16T00:00:00.000000+00:00",
    });
    apply(&mut store, "MESSAGE_CREATE", &raw.to_string());
    assert!(navigation.step(&store, unread));
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));
    // Already on the only unread one: nowhere else to go.
    assert!(!navigation.step(&store, unread));
    let back = Step {
        forward: false,
        ..unread
    };
    assert!(navigation.select_channel(&store, id(GENERAL)));
    assert!(navigation.step(&store, back));
    assert_eq!(navigation.selected_channel(), Some(id(VOICE)));
}

// ----- forum posts (milestone 49); every payload here is synthetic and unverified -----

/// Guild 100 owned by 2; the account (1) holds role 200 with `permissions`.
/// Channel 12 is a forum with two tags and REQUIRE_TAG, 13 a forum the account
/// cannot see.
fn forum_store(permissions: Permissions, threads: &[Value]) -> (Store, Navigation) {
    let raw = json!({
        "id": "100", "name": "G", "owner_id": "2", "member_count": 2,
        "roles": [
            {"id": "100", "name": "@everyone", "permissions": (Permissions::VIEW_CHANNEL
                | Permissions::READ_MESSAGE_HISTORY).0.to_string()},
            {"id": "200", "name": "r", "permissions": permissions.0.to_string()},
        ],
        "channels": [
            {"id": "10", "type": 0, "name": "general"},
            {"id": "12", "type": 15, "name": "help", "flags": 16, "default_sort_order": 1,
             "available_tags": [
                {"id": "7", "name": "bug", "moderated": false, "emoji_name": "x"},
                {"id": "8", "name": "staff", "moderated": true}]},
            {"id": "13", "type": 15, "name": "secret-forum", "permission_overwrites":
                [{"id": "100", "type": 0, "allow": "0", "deny": "1024"}]},
        ],
        "threads": threads,
        "members": [{"user": {"id": "1", "username": "me"}, "roles": ["200"]}],
    });
    let mut store = Store::new();
    let mut start = ready(NAV_READY);
    start.guilds.clear();
    start.unavailable_guilds.clear();
    replace(&mut store, start);
    apply(&mut store, "GUILD_CREATE", &raw.to_string());
    let mut navigation = Navigation::default();
    navigation.reconcile(&store);
    assert!(navigation.select_guild(&store, id(100)));
    (store, navigation)
}

fn post(id: u64, extra: Value) -> Value {
    thread_json(id, 11, 12, extra)
}

fn post_ids(navigation: &Navigation, store: &Store) -> Vec<u64> {
    navigation
        .forum_view(store)
        .map_or_else(Vec::new, |view| view.posts.iter().map(|p| p.id.0).collect())
}

#[test]
fn a_forum_lists_active_posts_pinned_then_newest_activity_and_not_in_the_sidebar() {
    let (store, mut navigation) = forum_store(
        ACCESS | Permissions::SEND_MESSAGES,
        &[
            post(
                40,
                json!({"last_message_id": "400", "message_count": 3, "owner_id": "1",
                    "applied_tags": ["7"]}),
            ),
            post(41, json!({"last_message_id": "900"})),
            post(42, json!({"thread_metadata": {"archived": true}})),
            post(43, json!({"flags": 2})),
            // a post under a forum the account cannot see, and one with no parent held
            thread_json(44, 11, 13, json!({})),
            thread_json(45, 11, 99, json!({})),
        ],
    );
    // Posts are not sidebar rows; the visible forum is.
    assert_eq!(ids(&rows(&navigation, &store)), [id(10), id(12)]);
    assert!(navigation.select_channel(&store, id(12)));
    let selection = navigation.selection(&store).unwrap();
    assert_eq!(selection.kind, ChannelKind::Forum);
    assert!(selection.permissions.send_messages);
    let view = navigation.forum_view(&store).unwrap();
    assert_eq!(
        post_ids(&navigation, &store),
        [43, 41, 40],
        "pinned, then activity"
    );
    assert_eq!(view.total, 3);
    assert!(view.require_tag && view.sort_creation && view.can_post && !view.media);
    assert_eq!(
        view.tags
            .iter()
            .map(|t| (t.id.0, t.name.as_str(), t.moderated))
            .collect::<Vec<_>>(),
        [(7, "bug", false), (8, "staff", true)]
    );
    let row = view.posts.iter().find(|p| p.id == id(40)).unwrap();
    assert_eq!(
        (row.replies, row.tags.clone(), row.owner),
        (3, vec![id(7)], Some(id(1)))
    );
    assert!(view.posts[0].pinned);
    // Not a forum: no view.
    assert!(navigation.select_channel(&store, id(10)));
    assert!(navigation.forum_view(&store).is_none());
    // The hidden forum and its posts stay closed.
    assert!(!navigation.select_channel(&store, id(13)));
    assert!(!navigation.select_channel(&store, id(44)));
    assert!(!navigation.select_channel(&store, id(45)));
}

#[test]
fn posting_needs_send_messages_in_the_forum_and_replying_send_in_threads() {
    let (store, mut navigation) = forum_store(ACCESS, &[post(40, json!({}))]);
    assert!(navigation.select_channel(&store, id(12)));
    assert!(!navigation.forum_view(&store).unwrap().can_post);
    let (store, mut navigation) = forum_store(
        Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY | Permissions::SEND_MESSAGES,
        &[post(40, json!({}))],
    );
    assert!(navigation.select_channel(&store, id(12)));
    assert!(navigation.forum_view(&store).unwrap().can_post);
    assert!(navigation.select_channel(&store, id(40)));
    assert!(
        !navigation
            .selection(&store)
            .unwrap()
            .permissions
            .send_messages,
        "SEND_MESSAGES does not reply in a post"
    );
}

#[test]
fn an_open_post_shows_under_its_forum_and_locked_or_archived_state_is_reported() {
    let (store, mut navigation) = forum_store(
        ACCESS,
        &[
            post(40, json!({})),
            post(41, json!({"thread_metadata": {"locked": true}})),
            post(42, json!({"thread_metadata": {"archived": true}})),
        ],
    );
    assert!(navigation.select_channel(&store, id(41)));
    let selection = navigation.selection(&store).unwrap();
    let state = selection.thread.unwrap();
    assert!(state.forum_post && state.locked && state.parent_id == id(12));
    assert!(!selection.permissions.send_messages, "locked");
    assert_eq!(ids(&rows(&navigation, &store)), [id(10), id(12), id(41)]);
    assert!(navigation.select_channel(&store, id(42)));
    let selection = navigation.selection(&store).unwrap();
    assert!(selection.thread.unwrap().archived && selection.permissions.send_messages);
    assert_eq!(ids(&rows(&navigation, &store)), [id(10), id(12), id(42)]);
    let (store, mut navigation) = forum_store(
        ACCESS | Permissions::MANAGE_THREADS,
        &[post(41, json!({"thread_metadata": {"locked": true}}))],
    );
    assert!(navigation.select_channel(&store, id(41)));
    assert!(
        navigation
            .selection(&store)
            .unwrap()
            .permissions
            .send_messages
    );
    assert!(navigation.select_channel(&store, id(12)));
    assert!(navigation.forum_view(&store).unwrap().can_manage);
}

#[test]
fn forum_events_converge_including_replayed_and_stale_ones() {
    let (mut store, mut navigation) = forum_store(ACCESS, &[post(40, json!({}))]);
    assert!(navigation.select_channel(&store, id(12)));
    let created = post(50, json!({"last_message_id": "500", "applied_tags": ["7"]})).to_string();
    apply(&mut store, "THREAD_CREATE", &created);
    apply(&mut store, "THREAD_CREATE", &created);
    assert_eq!(
        post_ids(&navigation, &store),
        [50, 40],
        "a replay adds nothing"
    );
    // A stale update without a newer marker never regresses the activity.
    apply(
        &mut store,
        "THREAD_UPDATE",
        &post(50, json!({"name": "renamed"})).to_string(),
    );
    let view = navigation.forum_view(&store).unwrap();
    let row = view.posts.iter().find(|p| p.id == id(50)).unwrap();
    assert_eq!(
        (row.title.as_str(), row.last_activity),
        ("renamed", id(500))
    );
    // Archiving removes the post from the active list; unarchiving restores it.
    let archived = post(50, json!({"thread_metadata": {"archived": true}})).to_string();
    apply(&mut store, "THREAD_UPDATE", &archived);
    assert_eq!(post_ids(&navigation, &store), [40]);
    apply(
        &mut store,
        "THREAD_UPDATE",
        &post(50, json!({})).to_string(),
    );
    assert_eq!(post_ids(&navigation, &store), [50, 40]);
    // Delete and a replayed delete.
    let delete = r#"{"id":"50","guild_id":"100","parent_id":"12","type":11}"#;
    apply(&mut store, "THREAD_DELETE", delete);
    apply(&mut store, "THREAD_DELETE", delete);
    assert_eq!(post_ids(&navigation, &store), [40]);
    // An open post that is deleted closes.
    assert!(navigation.select_channel(&store, id(40)));
    apply(
        &mut store,
        "THREAD_DELETE",
        r#"{"id":"40","guild_id":"100","parent_id":"12","type":11}"#,
    );
    navigation.reconcile(&store);
    assert!(navigation.selection(&store).is_none());
    assert_eq!(store.bytes(), store.recount());
    // THREAD_LIST_SYNC for the forum replaces its posts only.
    apply(
        &mut store,
        "THREAD_LIST_SYNC",
        &json!({"guild_id": "100", "channel_ids": ["12"], "threads": [post(60, json!({}))],
            "members": []})
        .to_string(),
    );
    assert!(navigation.select_channel(&store, id(12)));
    assert_eq!(post_ids(&navigation, &store), [60]);
}

#[test]
fn the_post_list_is_bounded() {
    let threads: Vec<_> = (0..MAX_FORUM_POSTS as u64 + 50)
        .map(|n| post(1000 + n, json!({})))
        .collect();
    let (store, mut navigation) = forum_store(ACCESS, &threads);
    assert!(navigation.select_channel(&store, id(12)));
    let view = navigation.forum_view(&store).unwrap();
    assert_eq!(view.posts.len(), MAX_FORUM_POSTS);
    assert_eq!(view.total, MAX_FORUM_POSTS + 50);
}

#[test]
fn forum_metadata_decodes_leniently_and_bounded() {
    let tags: Vec<_> = (1..=30)
        .map(|n| json!({"id": n.to_string(), "name": "t"}))
        .collect();
    let channel: Channel = serde_json::from_value(json!({
        "id": "12", "type": 16, "available_tags": tags,
        "applied_tags": ["1", "2", "3", "4", "5", "6", "7"],
        "flags": 16, "default_sort_order": 0, "message_count": 4,
    }))
    .unwrap();
    assert_eq!(channel.kind, fastcord_model::ChannelKind::GuildMedia);
    assert_eq!(channel.available_tags.len(), fastcord_model::MAX_FORUM_TAGS);
    assert_eq!(channel.applied_tags.len(), fastcord_model::MAX_APPLIED_TAGS);
    let nulls: Channel = serde_json::from_value(
        json!({"id": "12", "type": 15, "available_tags": null, "applied_tags": null}),
    )
    .unwrap();
    assert!(nulls.available_tags.is_empty() && nulls.applied_tags.is_empty());
    let plain: Channel = serde_json::from_value(json!({"id": "1", "type": 0})).unwrap();
    assert!(plain.available_tags.is_empty() && plain.flags == 0);
}
