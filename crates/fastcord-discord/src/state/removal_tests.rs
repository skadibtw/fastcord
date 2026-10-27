//! Every removal path must leave no dependent entry behind (#166): read
//! cursors, mutes and levels, calls, presences, users, voice states, stages,
//! activities, scheduled events and the consumer's asked-for members. Each
//! test seeds every dependent table for one entity, removes the entity with
//! its Gateway event and lists what survived.

use super::*;
use crate::gateway::{GuildMuteUpdate, decode_fixture};
use serde_json::{Value, json};

const G: u64 = 900;
const CATEGORY: u64 = 910;
const TEXT: u64 = 911;
const VOICE: u64 = 912;
const THREAD: u64 = 913;
const MEMBER: u64 = 1001;
const FRIEND: u64 = 10;
const DM: u64 = 300_000_000_000_000_001;
const GROUP: u64 = 300_000_000_000_000_002;

fn id(n: u64) -> Snowflake {
    Snowflake(n)
}

fn send(store: &mut Store, name: &str, raw: Value) -> Changes {
    let changes = store.apply(GatewayEvent::Dispatch {
        sequence: 1,
        event: decode_fixture(name, &raw.to_string()),
    });
    assert_eq!(
        store.bytes(),
        store.recount(),
        "ledger drifted after {name}"
    );
    changes
}

/// A store holding one guild with a category, a text channel, a voice channel
/// and a thread, one member who is in the voice channel, a friend, a DM and a
/// group DM, with every dependent table seeded for them.
fn world() -> Store {
    let mut store = Store::new();
    send(
        &mut store,
        "READY",
        json!({"session_id": "s", "resume_gateway_url": "wss://gateway.discord.gg",
            "user": {"id": "1", "username": "me"}, "guilds": [],
            "relationships": [{"id": FRIEND.to_string(), "type": 1,
                "user": {"id": FRIEND.to_string(), "username": "friend"}}]}),
    );
    send(
        &mut store,
        "GUILD_CREATE",
        json!({"id": G.to_string(), "name": "G", "owner_id": "1", "member_count": 2,
            "roles": [], "channels": [
                {"id": CATEGORY.to_string(), "type": 4, "name": "cat"},
                {"id": TEXT.to_string(), "type": 0, "name": "text", "parent_id": CATEGORY.to_string()},
                {"id": VOICE.to_string(), "type": 2, "name": "voice", "parent_id": CATEGORY.to_string()},
                {"id": THREAD.to_string(), "type": 11, "name": "thread", "parent_id": TEXT.to_string()}],
            "members": [{"user": {"id": MEMBER.to_string(), "username": "member"}, "roles": []}]}),
    );
    send(
        &mut store,
        "VOICE_STATE_UPDATE",
        json!({"guild_id": G.to_string(), "channel_id": VOICE.to_string(),
            "user_id": MEMBER.to_string(), "session_id": "v",
            "member": {"user": {"id": MEMBER.to_string(), "username": "member"}, "roles": []}}),
    );
    send(
        &mut store,
        "EMBEDDED_ACTIVITY_UPDATE",
        json!({"guild_id": G.to_string(), "channel_id": VOICE.to_string(),
            "embedded_activity": {"application_id": "5", "name": "act"},
            "participants": [{"user_id": MEMBER.to_string()}]}),
    );
    send(
        &mut store,
        "STAGE_INSTANCE_CREATE",
        json!({"guild_id": G.to_string(), "channel_id": VOICE.to_string(), "topic": "t"}),
    );
    send(
        &mut store,
        "GUILD_SCHEDULED_EVENT_CREATE",
        json!({"id": "7", "guild_id": G.to_string(), "channel_id": VOICE.to_string(),
            "creator_id": "9", "name": "ev", "scheduled_start_time": "2026-11-01T10:00:00+00:00",
            "status": 1, "entity_type": 2}),
    );
    seed_read(&mut store, Some(id(G)), &[TEXT, VOICE, THREAD, CATEGORY]);
    store
        .focus
        .asked
        .entry(id(G))
        .or_default()
        .insert(id(MEMBER));
    for user in [MEMBER, FRIEND] {
        send(
            &mut store,
            "PRESENCE_UPDATE",
            json!({"user": {"id": user.to_string()}, "status": "online", "activities": []}),
        );
    }
    send(
        &mut store,
        "CHANNEL_CREATE",
        json!({"id": DM.to_string(), "type": 1,
            "recipients": [{"id": FRIEND.to_string(), "username": "friend"}]}),
    );
    send(
        &mut store,
        "CHANNEL_CREATE",
        json!({"id": GROUP.to_string(), "type": 3, "owner_id": "1",
            "recipients": [{"id": "2000", "username": "pal"}, {"id": "2001", "username": "pal2"}]}),
    );
    seed_read(&mut store, None, &[DM, GROUP]);
    for call in [DM, GROUP] {
        send(
            &mut store,
            "CALL_CREATE",
            json!({"channel_id": call.to_string(), "ringing": [], "voice_states": []}),
        );
    }
    send(
        &mut store,
        "PRESENCE_UPDATE",
        json!({"user": {"id": "2000"}, "status": "idle", "activities": []}),
    );
    store
}

fn seed_read(store: &mut Store, guild: Option<Snowflake>, channels: &[u64]) {
    let ids: Vec<Snowflake> = channels.iter().copied().map(id).collect();
    for &channel in &ids {
        assert!(store.read.ack(channel, id(50), 2));
    }
    store.read.update_mutes(
        guild,
        guild.is_some(),
        &ids.iter().map(|&c| (c, true)).collect::<Vec<_>>(),
    );
    store.read.update_levels(&GuildMuteUpdate {
        guild_id: guild,
        guild_muted: false,
        channels: Vec::new(),
        level: Some(NotifyLevel::Mentions),
        suppress_everyone: true,
        suppress_roles: true,
        channel_levels: ids
            .iter()
            .map(|&c| (c, Some(NotifyLevel::Nothing)))
            .collect(),
    });
}

fn channel_leftovers(store: &Store, channel: u64, found: &mut Vec<String>) {
    let c = id(channel);
    if store.acked(c).is_some() {
        found.push(format!("read cursor {channel}"));
    }
    if store.channel_muted(c) {
        found.push(format!("channel mute {channel}"));
    }
    if store.channel_notify_level(c).is_some() {
        found.push(format!("channel level {channel}"));
    }
    if store.calls().iter().any(|call| call.channel_id == c) {
        found.push(format!("call {channel}"));
    }
}

fn guild_leftovers(store: &Store, found: &mut Vec<String>) {
    if store.guild_muted(id(G)) {
        found.push("guild mute".into());
    }
    if store.guild_notify_level(id(G)).is_some() {
        found.push("guild level".into());
    }
    if store.suppressions(id(G)) != (false, false) {
        found.push("guild suppressions".into());
    }
    if store.focus.asked.contains_key(&id(G)) {
        found.push("focus.asked guild".into());
    }
}

fn user_leftovers(store: &Store, user: u64, found: &mut Vec<String>) {
    if store.user(id(user)).is_some() {
        found.push(format!("user {user}"));
    }
    if store.presences().contains_key(&id(user)) {
        found.push(format!("presence {user}"));
    }
}

/// Entries of a channel that live inside its guild.
fn inside_leftovers(store: &Store, channel: u64, found: &mut Vec<String>) {
    let Some(guild) = store.guild(id(G)) else {
        return;
    };
    if guild
        .voice_states()
        .any(|s| s.channel_id == Some(id(channel)))
    {
        found.push(format!("voice state in {channel}"));
    }
    if guild.stages.iter().any(|s| s.channel_id == id(channel)) {
        found.push(format!("stage {channel}"));
    }
    if guild.activities(id(channel)).next().is_some() {
        found.push(format!("activity {channel}"));
    }
    if guild
        .scheduled_events()
        .iter()
        .any(|e| e.channel_id == Some(id(channel)))
    {
        found.push(format!("event in {channel}"));
    }
}

#[track_caller]
fn none(found: Vec<String>) {
    assert_eq!(found, Vec::<String>::new(), "dependent entries survived");
}

#[test]
fn the_world_seeds_every_table() {
    let store = world();
    let mut found = Vec::new();
    for channel in [TEXT, VOICE, THREAD] {
        channel_leftovers(&store, channel, &mut found);
    }
    guild_leftovers(&store, &mut found);
    inside_leftovers(&store, VOICE, &mut found);
    user_leftovers(&store, MEMBER, &mut found);
    // 3 channels x (cursor, mute, level), 4 guild entries, 4 inside, 2 user.
    assert_eq!(found.len(), 3 * 3 + 4 + 4 + 2, "{found:?}");
}

#[test]
fn guild_leave_clears_everything_that_belongs_to_it() {
    let mut store = world();
    send(&mut store, "GUILD_DELETE", json!({"id": G.to_string()}));
    let mut found = Vec::new();
    for channel in [TEXT, VOICE, THREAD, CATEGORY] {
        channel_leftovers(&store, channel, &mut found);
    }
    guild_leftovers(&store, &mut found);
    user_leftovers(&store, MEMBER, &mut found);
    none(found);
}

#[test]
fn a_guild_outage_drops_its_entry_but_keeps_the_account_state_it_will_need_back() {
    let mut store = world();
    send(
        &mut store,
        "GUILD_DELETE",
        json!({"id": G.to_string(), "unavailable": true}),
    );
    let mut found = Vec::new();
    user_leftovers(&store, MEMBER, &mut found);
    none(found);
    assert!(store.acked(id(TEXT)).is_some(), "cursors survive an outage");
    assert!(store.guild_muted(id(G)));
}

#[test]
fn deleting_a_text_channel_clears_it_and_its_threads() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_DELETE",
        json!({"id": TEXT.to_string(), "type": 0, "guild_id": G.to_string()}),
    );
    let mut found = Vec::new();
    for channel in [TEXT, THREAD] {
        channel_leftovers(&store, channel, &mut found);
    }
    none(found);
}

#[test]
fn deleting_a_voice_channel_clears_what_lives_in_it() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_DELETE",
        json!({"id": VOICE.to_string(), "type": 2, "guild_id": G.to_string()}),
    );
    let mut found = Vec::new();
    channel_leftovers(&store, VOICE, &mut found);
    inside_leftovers(&store, VOICE, &mut found);
    none(found);
}

#[test]
fn deleting_a_thread_clears_it() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_DELETE",
        json!({"id": THREAD.to_string(), "type": 11, "guild_id": G.to_string(),
            "parent_id": TEXT.to_string()}),
    );
    let mut found = Vec::new();
    channel_leftovers(&store, THREAD, &mut found);
    none(found);
}

#[test]
fn removing_a_member_clears_their_voice_and_asked_entries() {
    let mut store = world();
    send(
        &mut store,
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id": G.to_string(), "user": {"id": MEMBER.to_string()}}),
    );
    let mut found = Vec::new();
    if store
        .guild(id(G))
        .unwrap()
        .voice_states()
        .any(|s| s.user_id == id(MEMBER))
    {
        found.push("voice state of member".into());
    }
    user_leftovers(&store, MEMBER, &mut found);
    if store.focus.asked[&id(G)].contains(&id(MEMBER)) {
        found.push("focus.asked member".into());
    }
    none(found);
}

#[test]
fn closing_a_dm_clears_it() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_DELETE",
        json!({"id": DM.to_string(), "type": 1}),
    );
    let mut found = Vec::new();
    channel_leftovers(&store, DM, &mut found);
    none(found);
    // The friend is still a friend: their presence stays.
    assert!(store.presences().contains_key(&id(FRIEND)));
}

#[test]
fn leaving_a_group_dm_clears_it_and_its_recipients() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_RECIPIENT_REMOVE",
        json!({"channel_id": GROUP.to_string(), "user": {"id": "1"}}),
    );
    assert!(store.private_channel(id(GROUP)).is_none());
    let mut found = Vec::new();
    channel_leftovers(&store, GROUP, &mut found);
    user_leftovers(&store, 2000, &mut found);
    none(found);
}

#[test]
fn deleting_a_group_dm_clears_it_and_its_recipients() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_DELETE",
        json!({"id": GROUP.to_string(), "type": 3}),
    );
    let mut found = Vec::new();
    channel_leftovers(&store, GROUP, &mut found);
    user_leftovers(&store, 2000, &mut found);
    none(found);
}

#[test]
fn removing_a_relationship_clears_the_presence_and_user_it_alone_held() {
    let mut store = world();
    send(
        &mut store,
        "CHANNEL_DELETE",
        json!({"id": DM.to_string(), "type": 1}),
    );
    send(
        &mut store,
        "RELATIONSHIP_REMOVE",
        json!({"id": FRIEND.to_string(), "type": 1}),
    );
    let mut found = Vec::new();
    user_leftovers(&store, FRIEND, &mut found);
    none(found);
}

#[test]
fn deleting_a_role_clears_it_from_members_and_overwrites() {
    let mut store = world();
    send(
        &mut store,
        "GUILD_ROLE_CREATE",
        json!({"guild_id": G.to_string(), "role": {"id": "77", "name": "r", "position": 1,
            "permissions": "0"}}),
    );
    send(
        &mut store,
        "GUILD_MEMBER_UPDATE",
        json!({"guild_id": G.to_string(), "user": {"id": MEMBER.to_string()}, "roles": ["77"]}),
    );
    send(
        &mut store,
        "CHANNEL_UPDATE",
        json!({"id": TEXT.to_string(), "type": 0, "guild_id": G.to_string(), "name": "text",
            "permission_overwrites": [{"id": "77", "type": 0, "allow": "1024", "deny": "0"}]}),
    );
    let guild = store.guild(id(G)).unwrap();
    assert!(guild.member(id(MEMBER)).unwrap().roles.contains(&id(77)));
    assert_eq!(
        guild.channel(id(TEXT)).unwrap().permission_overwrites.len(),
        1
    );
    send(
        &mut store,
        "GUILD_ROLE_DELETE",
        json!({"guild_id": G.to_string(), "role_id": "77"}),
    );
    let guild = store.guild(id(G)).unwrap();
    assert!(guild.member(id(MEMBER)).unwrap().roles.is_empty());
    assert!(
        guild
            .channel(id(TEXT))
            .unwrap()
            .permission_overwrites
            .is_empty()
    );
}
