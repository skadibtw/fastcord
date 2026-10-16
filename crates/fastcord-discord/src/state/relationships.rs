//! Friends, blocks and pending requests (SPEC §16 milestone 45).
//!
//! Kept apart from the shared user table: a relationship's user must outlive
//! the sweep of users nobody else references. At most [`MAX_RELATIONSHIPS`]
//! are held (later ones are refused, existing ones always update), and the
//! name-sorted view the UI reads is rebuilt once per change and shared.

use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::Arc;

use fastcord_model::{Relationship, RelationshipKind, Snowflake};

use super::user_bytes;
use crate::gateway::{MAX_RELATIONSHIPS, RelationshipUpdate};

#[derive(Debug, Default)]
pub(super) struct Relationships {
    map: BTreeMap<Snowflake, Relationship>,
    rows: Arc<[Relationship]>,
    bytes: usize,
}

fn cost(relationship: &Relationship) -> usize {
    // Held twice: in the map and in the shared view.
    2 * (size_of::<Relationship>() + user_bytes(&relationship.user)) + 48
}

impl Relationships {
    pub(super) fn replace(&mut self, all: Vec<Relationship>) {
        *self = Self::default();
        for relationship in all {
            self.insert(relationship);
        }
        self.rebuild();
    }

    fn insert(&mut self, relationship: Relationship) -> bool {
        let id = relationship.user.id;
        if self.map.len() >= MAX_RELATIONSHIPS && !self.map.contains_key(&id) {
            return false;
        }
        self.bytes += cost(&relationship);
        if let Some(old) = self.map.insert(id, relationship) {
            self.bytes -= cost(&old);
        }
        true
    }

    /// Returns whether anything changed.
    pub(super) fn add(&mut self, relationship: Relationship) -> bool {
        if self.map.get(&relationship.user.id) == Some(&relationship) || !self.insert(relationship)
        {
            return false;
        }
        self.rebuild();
        true
    }

    /// A stale update for a relationship that is gone changes nothing.
    pub(super) fn update(&mut self, update: RelationshipUpdate) -> bool {
        let Some(kind) = update.kind else {
            return false;
        };
        match self.map.get_mut(&update.id) {
            Some(held) if held.kind != kind => held.kind = kind,
            _ => return false,
        }
        self.rebuild();
        true
    }

    pub(super) fn remove(&mut self, id: Snowflake) -> bool {
        let Some(old) = self.map.remove(&id) else {
            return false;
        };
        self.bytes -= cost(&old);
        self.rebuild();
        true
    }

    pub(super) fn kind(&self, id: Snowflake) -> Option<RelationshipKind> {
        self.map.get(&id).map(|held| held.kind)
    }

    pub(super) fn ids(&self) -> impl Iterator<Item = Snowflake> + '_ {
        self.map.keys().copied()
    }

    pub(super) fn rows(&self) -> Arc<[Relationship]> {
        Arc::clone(&self.rows)
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

    /// Sorted by kind, then name, then ID.
    fn rebuild(&mut self) {
        let mut rows: Vec<Relationship> = self.map.values().cloned().collect();
        rows.sort_by_cached_key(|row| {
            (
                row.kind,
                row.user.display_name().to_lowercase(),
                row.user.id,
            )
        });
        self.rows = rows.into();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Changes, Store};
    use crate::gateway::{GatewayEvent, MAX_RELATIONSHIPS, decode_fixture};
    use fastcord_model::{RelationshipKind, Snowflake};

    /// Synthetic payloads in the shape of observed traffic; unverified live.
    const READY: &str = r#"{"session_id":"s","resume_gateway_url":"wss://gateway.discord.gg",
        "user":{"id":"1","username":"me"},"guilds":[],"relationships":[
        {"id":"10","type":1,"user":{"id":"10","username":"Zed"}},
        {"id":"11","type":3,"user":{"id":"11","username":"amy","global_name":"Amy"}},
        {"id":"12","type":2,"user":{"id":"12","username":"blocked_one"}},
        {"id":"13","type":4,"user":{"id":"13","username":"pending"}},
        {"id":"14","type":5,"user":{"id":"14","username":"implicit"}},
        {"id":"15","type":1},
        {"id":"99","type":1,"user":{"id":"16","username":"mismatch"}},
        "junk", null]}"#;

    fn apply(store: &mut Store, name: &str, raw: &str) -> Changes {
        let changes = store.apply(GatewayEvent::Dispatch {
            sequence: 1,
            event: decode_fixture(name, raw),
        });
        assert_eq!(store.bytes(), store.recount(), "ledger drifted");
        changes
    }

    fn names(store: &Store) -> Vec<(String, RelationshipKind)> {
        store
            .relationships()
            .iter()
            .map(|r| (r.user.display_name().to_owned(), r.kind))
            .collect()
    }

    #[test]
    fn ready_keeps_only_well_formed_kinds_sorted_and_odd_entries_are_skipped() {
        let mut store = Store::new();
        assert!(apply(&mut store, "READY", READY).relationships);
        use RelationshipKind::*;
        assert_eq!(
            names(&store),
            [
                ("Zed".into(), Friend),
                ("blocked_one".into(), Blocked),
                ("Amy".into(), IncomingRequest),
                ("pending".into(), OutgoingRequest),
            ]
        );
        assert_eq!(store.relationship(Snowflake(11)), Some(IncomingRequest));
        assert_eq!(store.relationship(Snowflake(14)), None);
    }

    #[test]
    fn events_converge_and_stale_ones_change_nothing() {
        let mut store = Store::new();
        apply(&mut store, "READY", READY);
        let added = r#"{"id":"20","type":3,"user":{"id":"20","username":"newbie"}}"#;
        assert!(apply(&mut store, "RELATIONSHIP_ADD", added).relationships);
        // A replayed event is not a change.
        assert!(!apply(&mut store, "RELATIONSHIP_ADD", added).relationships);
        assert!(apply(&mut store, "RELATIONSHIP_UPDATE", r#"{"id":"20","type":1}"#).relationships);
        assert_eq!(
            store.relationship(Snowflake(20)),
            Some(RelationshipKind::Friend)
        );
        // A nickname-only update, an unknown ID and a repeat are all no-ops.
        for stale in [
            r#"{"id":"20","nickname":"n"}"#,
            r#"{"id":"77","type":1}"#,
            r#"{"id":"20","type":1}"#,
        ] {
            let changes = apply(&mut store, "RELATIONSHIP_UPDATE", stale);
            assert!(!changes.relationships, "{stale}");
        }
        assert!(apply(&mut store, "RELATIONSHIP_REMOVE", r#"{"id":"20","type":1}"#).relationships);
        assert!(!apply(&mut store, "RELATIONSHIP_REMOVE", r#"{"id":"20","type":1}"#).relationships);
        assert_eq!(store.relationships_held(), 4);
    }

    #[test]
    fn a_new_ready_replaces_and_a_flood_is_bounded() {
        let mut store = Store::new();
        apply(&mut store, "READY", READY);
        let empty = r#"{"session_id":"s2","resume_gateway_url":"wss://gateway.discord.gg","user":{"id":"1","username":"me"},"guilds":[],"relationships":null}"#;
        apply(&mut store, "READY", empty);
        assert_eq!(store.relationships_held(), 0);
        for index in 0..(MAX_RELATIONSHIPS as u64 + 50) {
            let add = format!(
                r#"{{"id":"{0}","type":1,"user":{{"id":"{0}","username":"u{0}"}}}}"#,
                1000 + index
            );
            apply(&mut store, "RELATIONSHIP_ADD", &add);
        }
        assert_eq!(store.relationships_held(), MAX_RELATIONSHIPS);
    }
}
