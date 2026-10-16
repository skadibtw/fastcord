//! Presence of referenced users (SPEC §16 milestone 46).
//!
//! Only friends, blocked/pending users, direct-conversation recipients and the
//! account itself are admitted (the caller decides), at most [`MAX_PRESENCES`].
//! Entries of users who stop being referenced are pruned. The map is shared
//! with UI snapshots; a write while a snapshot holds it copies it once
//! (bounded by the cap), and the app reconciles snapshots coalesced.

use std::collections::HashMap;
use std::mem::size_of;
use std::sync::Arc;

use fastcord_model::{Presence, Snowflake};

use crate::gateway::{MAX_PRESENCES, PresenceUpdate};

#[derive(Debug, Default)]
pub(super) struct Presences {
    map: Arc<HashMap<Snowflake, Presence>>,
    bytes: usize,
}

fn cost(presence: &Presence) -> usize {
    // Held twice at worst: live map and a snapshot's copy.
    2 * (size_of::<Presence>() + presence.custom.as_ref().map_or(0, String::capacity)) + 64
}

impl Presences {
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Merges a whole or partial update. Returns whether anything changed.
    pub(super) fn apply(&mut self, update: &PresenceUpdate) -> bool {
        let held = self.map.get(&update.user);
        if held.is_none() && self.map.len() >= MAX_PRESENCES {
            return false;
        }
        let mut next = held.cloned().unwrap_or_default();
        if let Some(status) = update.status {
            next.status = status;
        }
        if let Some(custom) = &update.custom {
            next.custom = custom.clone();
        }
        if held == Some(&next) {
            return false;
        }
        self.bytes += cost(&next);
        if let Some(old) = Arc::make_mut(&mut self.map).insert(update.user, next) {
            self.bytes -= cost(&old);
        }
        true
    }

    /// Drops entries whose user is no longer referenced.
    pub(super) fn retain(&mut self, keep: impl Fn(Snowflake) -> bool) -> bool {
        if self.map.keys().all(|id| keep(*id)) {
            return false;
        }
        let map = Arc::make_mut(&mut self.map);
        map.retain(|id, _| keep(*id));
        self.bytes = map.values().map(cost).sum();
        true
    }

    pub(super) fn shared(&self) -> Arc<HashMap<Snowflake, Presence>> {
        Arc::clone(&self.map)
    }

    pub(super) fn len(&self) -> usize {
        self.map.len()
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(super) fn recount(&self) -> usize {
        self.map.values().map(cost).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Changes, Store};
    use crate::gateway::{GatewayEvent, MAX_PRESENCES, decode_fixture};
    use fastcord_model::{Snowflake, Status};

    /// Synthetic payloads in the shape of observed traffic; unverified live.
    /// 10 is a friend, 30 a DM recipient, 40 a stranger (no reference).
    const READY: &str = r#"{"session_id":"s","resume_gateway_url":"wss://gateway.discord.gg",
        "user":{"id":"1","username":"me"},"guilds":[],
        "users":[{"id":"30","username":"dm"}],
        "private_channels":[{"id":"300","type":1,"recipient_ids":["30"]}],
        "relationships":[{"id":"10","type":1,"user":{"id":"10","username":"Zed"}}],
        "presences":[
          {"user":{"id":"10"},"status":"idle","activities":[
            {"type":0,"name":"game"},{"type":4,"name":"Custom Status","state":"brb"}]},
          {"user":{"id":"30"},"status":"dnd","activities":[]},
          {"user":{"id":"40"},"status":"online"},
          {"user":{"id":"1"},"status":"invisible"},
          "junk", {"status":"online"}]}"#;

    fn apply(store: &mut Store, name: &str, raw: &str) -> Changes {
        let changes = store.apply(GatewayEvent::Dispatch {
            sequence: 1,
            event: decode_fixture(name, raw),
        });
        assert_eq!(store.bytes(), store.recount(), "ledger drifted");
        changes
    }

    fn held(store: &Store, id: u64) -> Option<(Status, Option<String>)> {
        store
            .presences()
            .get(&Snowflake(id))
            .map(|p| (p.status, p.custom.clone()))
    }

    fn update(store: &mut Store, raw: &str) -> bool {
        apply(store, "PRESENCE_UPDATE", raw).presence
    }

    #[test]
    fn ready_keeps_only_referenced_users_and_reads_custom_status() {
        let mut store = Store::new();
        apply(&mut store, "READY", READY);
        assert_eq!(held(&store, 10), Some((Status::Idle, Some("brb".into()))));
        assert_eq!(held(&store, 30), Some((Status::Dnd, None)));
        assert_eq!(held(&store, 1), Some((Status::Invisible, None)));
        assert_eq!(held(&store, 40), None, "strangers are not tracked");
        assert_eq!(store.presences_held(), 3);
    }

    #[test]
    fn partial_updates_merge_and_repeats_change_nothing() {
        let mut store = Store::new();
        apply(&mut store, "READY", READY);
        // Status only: the custom text stays.
        assert!(update(
            &mut store,
            r#"{"user":{"id":"10"},"status":"online"}"#
        ));
        assert_eq!(held(&store, 10), Some((Status::Online, Some("brb".into()))));
        assert!(!update(
            &mut store,
            r#"{"user":{"id":"10"},"status":"online"}"#
        ));
        // Activities only, none custom: the text clears, the status stays.
        assert!(update(&mut store, r#"{"user_id":"10","activities":[]}"#));
        assert_eq!(held(&store, 10), Some((Status::Online, None)));
        // An unknown status string is "omitted", not an error.
        assert!(!update(
            &mut store,
            r#"{"user":{"id":"10"},"status":"vibing"}"#
        ));
        // Unreferenced users are ignored.
        assert!(!update(
            &mut store,
            r#"{"user":{"id":"77"},"status":"online"}"#
        ));
        assert_eq!(held(&store, 77), None);
    }

    #[test]
    fn unreferenced_users_are_pruned_and_ready_replaces() {
        let mut store = Store::new();
        apply(&mut store, "READY", READY);
        let removed = apply(&mut store, "RELATIONSHIP_REMOVE", r#"{"id":"10","type":1}"#);
        assert!(removed.presence);
        assert_eq!(held(&store, 10), None);
        let deleted = r#"{"id":"300","type":1,"recipient_ids":["30"]}"#;
        assert!(apply(&mut store, "CHANNEL_DELETE", deleted).presence);
        assert_eq!(held(&store, 30), None);
        assert_eq!(store.presences_held(), 1, "only the account itself");
        let empty = r#"{"session_id":"s2","resume_gateway_url":"wss://gateway.discord.gg","user":{"id":"1","username":"me"},"guilds":[],"presences":null}"#;
        apply(&mut store, "READY", empty);
        assert_eq!(store.presences_held(), 0);
    }

    #[test]
    fn supplemental_friend_presences_apply_and_a_flood_is_bounded() {
        let mut store = Store::new();
        apply(&mut store, "READY", READY);
        let supplemental = r#"{"guilds":[],"merged_presences":{"friends":[
            {"user_id":"10","status":"dnd","activities":[]}],"guilds":[]}}"#;
        assert!(apply(&mut store, "READY_SUPPLEMENTAL", supplemental).presence);
        assert_eq!(held(&store, 10), Some((Status::Dnd, None)));
        let total = MAX_PRESENCES as u64 + 50;
        let friends: Vec<String> = (0..total)
            .map(|n| {
                format!(
                    r#"{{"id":"{0}","type":1,"user":{{"id":"{0}","username":"u{0}"}}}}"#,
                    1000 + n
                )
            })
            .collect();
        // MAX_RELATIONSHIPS equals MAX_PRESENCES, so a few friends are dropped
        // already; the presence cap is what is under test.
        let ready = format!(
            r#"{{"session_id":"s","resume_gateway_url":"wss://gateway.discord.gg","user":{{"id":"1","username":"me"}},"guilds":[],"relationships":[{}]}}"#,
            friends.join(",")
        );
        apply(&mut store, "READY", &ready);
        for n in 0..total {
            update(
                &mut store,
                &format!(r#"{{"user":{{"id":"{}"}},"status":"online"}}"#, 1000 + n),
            );
        }
        assert_eq!(store.presences_held(), MAX_PRESENCES);
    }
}
