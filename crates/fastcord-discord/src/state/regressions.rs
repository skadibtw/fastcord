use super::*;
use crate::gateway::{MemberListId, decode_fixture};

const GUILD: Snowflake = Snowflake(41_771_983_423_143_937);
const CHANNEL: Snowflake = Snowflake(41_771_983_423_143_938);
const USER: Snowflake = Snowflake(80_351_110_224_678_912);
const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
const LIST: &str = include_str!("../../../../fixtures/gateway/member_list_update_sync.json");
const MEMBER: &str = include_str!("../../../../fixtures/gateway/guild_member_update.json");

fn apply(store: &mut Store, name: &str, raw: &str) {
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: decode_fixture(name, raw),
    });
    assert_eq!(store.bytes(), store.recount());
}

fn store() -> Store {
    let mut store = Store::new();
    apply(&mut store, "READY", READY);
    let mut target = SubscriptionTarget::default();
    target.select_channel(GUILD, CHANNEL);
    store.focus(&target);
    store
}

#[test]
fn partial_member_and_user_updates_preserve_absence_and_clear_explicit_null() {
    let mut store = store();
    apply(&mut store, "GUILD_MEMBER_UPDATE", MEMBER);
    let previous = store.guild(GUILD).unwrap().member(USER).unwrap().clone();
    let previous_user = store.user(USER).unwrap().clone();
    apply(
        &mut store,
        "GUILD_MEMBER_UPDATE",
        &format!(r#"{{"guild_id":"{GUILD}","user":{{"id":"{USER}"}}}}"#),
    );
    assert_eq!(store.guild(GUILD).unwrap().member(USER), Some(&previous));
    assert_eq!(store.user(USER), Some(&previous_user));
    apply(
        &mut store,
        "GUILD_MEMBER_UPDATE",
        &format!(
            r#"{{"guild_id":"{GUILD}","user":{{"id":"{USER}","global_name":null,"avatar":null}},"nick":null,"roles":[],"communication_disabled_until":null}}"#
        ),
    );
    let member = store.guild(GUILD).unwrap().member(USER).unwrap();
    assert!(member.nick.is_none());
    assert!(member.roles.is_empty());
    assert!(member.communication_disabled_until.is_none());
    let user = store.user(USER).unwrap();
    assert_eq!(user.username, previous_user.username);
    assert!(user.global_name.is_none());
    assert!(user.avatar.is_none());
}

#[test]
fn startup_users_survive_until_deduplicated_supplemental_members_reference_them() {
    let Dispatch::Ready(mut ready) = decode_fixture("READY", READY) else {
        panic!()
    };
    let fresh = Snowflake(12345);
    ready.users.push(User {
        id: fresh,
        username: "supplemental-only".into(),
        global_name: None,
        avatar: None,
        bot: false,
    });
    let mut store = Store::new();
    store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: Dispatch::Ready(ready),
    });
    assert_eq!(store.user(fresh).unwrap().username, "supplemental-only");
    apply(
        &mut store,
        "READY_SUPPLEMENTAL",
        &format!(
            r#"{{"guilds":[{{"id":"{GUILD}"}}],"merged_members":[[{{"user_id":"{fresh}","roles":[]}}]]}}"#
        ),
    );
    assert!(store.guild(GUILD).unwrap().member(fresh).is_some());
    assert_eq!(store.user(fresh).unwrap().username, "supplemental-only");
}

#[test]
fn wrong_channel_member_lists_never_replace_the_visible_list() {
    let mut store = store();
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    let before = store.guild(GUILD).unwrap().member_list().unwrap().clone();
    let stale = LIST.replace("everyone", "old-permission-list");
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &stale);
    assert_eq!(store.guild(GUILD).unwrap().member_list(), Some(&before));
    assert_eq!(before.id(), &MemberListId("everyone".into()));
}

#[test]
fn permission_list_keys_follow_the_selected_channel_not_last_arrival() {
    let mut store = store();
    apply(
        &mut store,
        "CHANNEL_CREATE",
        &format!(
            r#"{{"id":"77","guild_id":"{GUILD}","type":0,"name":"private","permission_overwrites":[{{"id":"{GUILD}","type":0,"allow":"0","deny":"1024"}},{{"id":"123","type":0,"allow":"1024","deny":"0"}}]}}"#
        ),
    );
    let guild = store.guild(GUILD).unwrap();
    let key = list_id::for_channel(GUILD, guild.roles(), guild.channel(Snowflake(77)).unwrap());
    assert_ne!(key.0, "everyone");
    let mut target = SubscriptionTarget::default();
    target.select_channel(GUILD, Snowflake(77));
    store.focus(&target);
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    assert!(store.guild(GUILD).unwrap().member_list().is_none());
    let private = LIST.replace("everyone", &key.0);
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &private);
    assert_eq!(
        store.guild(GUILD).unwrap().member_list().unwrap().id(),
        &key
    );
    target.select_channel(GUILD, CHANNEL);
    store.focus(&target);
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &private);
    assert!(store.guild(GUILD).unwrap().member_list().is_none());
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    assert_eq!(
        store.guild(GUILD).unwrap().member_list().unwrap().id().0,
        "everyone"
    );
}

#[test]
fn viewport_changes_release_ranges_and_late_sync_cannot_restore_them() {
    let mut store = store();
    let mut target = SubscriptionTarget::default();
    target.select_channel(GUILD, CHANNEL);
    target.set_viewport(200, 299);
    store.focus(&target);
    let range = format!(
        r#"{{"id":"everyone","guild_id":"{GUILD}","ops":[{{"op":"SYNC","range":[200,299],"items":[{{"member":{{"user_id":"555","roles":[]}}}}]}}]}}"#
    );
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &range);
    assert!(
        store
            .guild(GUILD)
            .unwrap()
            .member_list()
            .unwrap()
            .row(200)
            .is_some()
    );
    target.set_viewport(500, 599);
    let changes = store.focus(&target);
    assert!(changes.member_lists.contains(&GUILD));
    assert!(
        store
            .guild(GUILD)
            .unwrap()
            .member_list()
            .unwrap()
            .row(200)
            .is_none()
    );
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", &range);
    assert!(
        store
            .guild(GUILD)
            .unwrap()
            .member_list()
            .unwrap()
            .row(200)
            .is_none()
    );
    assert_eq!(store.bytes(), store.recount());
}

#[test]
fn zero_counts_and_empty_groups_replace_old_header_state() {
    let mut store = store();
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    apply(
        &mut store,
        "GUILD_MEMBER_LIST_UPDATE",
        &format!(
            r#"{{"id":"everyone","guild_id":"{GUILD}","member_count":0,"online_count":0,"groups":[],"ops":[{{"op":"SYNC","range":[0,99],"items":[]}}]}}"#
        ),
    );
    let guild = store.guild(GUILD).unwrap();
    let list = guild.member_list().unwrap();
    assert_eq!(guild.member_count(), 0);
    assert_eq!(list.member_count(), 0);
    assert_eq!(list.online_count(), 0);
    assert!(list.groups().is_empty());
    assert_eq!(list.known_rows(), 0);
}

#[test]
fn permission_metadata_changes_release_stale_rows_before_replacement_sync() {
    let mut store = store();
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    apply(
        &mut store,
        "CHANNEL_UPDATE",
        &format!(
            r#"{{"id":"{CHANNEL}","guild_id":"{GUILD}","type":0,"name":"now-private","permission_overwrites":[{{"id":"{GUILD}","type":0,"allow":"0","deny":"1024"}}]}}"#
        ),
    );
    assert!(store.guild(GUILD).unwrap().member_list().is_none());
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    assert!(store.guild(GUILD).unwrap().member_list().is_none());
    // Outage/recreation must not retain the old permission-list key.
    apply(
        &mut store,
        "GUILD_DELETE",
        &format!(r#"{{"id":"{GUILD}","unavailable":true}}"#),
    );
    apply(
        &mut store,
        "GUILD_CREATE",
        &format!(
            r#"{{"id":"{GUILD}","name":"Returned","roles":[{{"id":"{GUILD}","name":"@everyone","permissions":"1024","position":0}}],"channels":[{{"id":"{CHANNEL}","type":0,"name":"public","permission_overwrites":[]}}]}}"#
        ),
    );
    apply(&mut store, "GUILD_MEMBER_LIST_UPDATE", LIST);
    assert_eq!(
        store.guild(GUILD).unwrap().member_list().unwrap().id().0,
        "everyone"
    );
}
