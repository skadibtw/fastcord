//! Friends, blocks and pending requests (SPEC §16 milestone 45).
//!
//! Kept apart from the shared user table: a relationship's user must outlive
//! the sweep of users nobody else references. At most [`MAX_RELATIONSHIPS`]
//! are held (later ones are refused, existing ones always update), and the
//! name-sorted view the UI reads is rebuilt once per change and shared.

use std::collections::{BTreeMap, BTreeSet};
use std::mem::size_of;
use std::sync::Arc;

use fastcord_model::{Relationship, RelationshipKind, Snowflake};

use super::user_bytes;
use crate::gateway::{MAX_RELATIONSHIPS, RelationshipUpdate};

/// Why a user's content is withheld. Blocking wins over ignoring; both hide
/// messages, mentions, notifications and calls (one policy, read by every
/// consumer through [`Store::suppression`](super::Store::suppression)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suppression {
    Ignored,
    Blocked,
}

/// Bytes charged for one ignored ID (held in the set and the shared view).
const IGNORED_COST: usize = 2 * (size_of::<Snowflake>() + size_of::<Suppression>()) + 48;

#[derive(Debug, Default)]
pub(super) struct Relationships {
    map: BTreeMap<Snowflake, Relationship>,
    /// Ignored users, with or without a relationship of a kept kind.
    ignored: BTreeSet<Snowflake>,
    rows: Arc<[Relationship]>,
    suppressed: Arc<BTreeMap<Snowflake, Suppression>>,
    bytes: usize,
}

fn cost(relationship: &Relationship) -> usize {
    // Held twice: in the map and in the shared view.
    2 * (size_of::<Relationship>() + user_bytes(&relationship.user)) + 48
}

impl Relationships {
    pub(super) fn replace(&mut self, all: Vec<Relationship>, ignored_only: Vec<Snowflake>) {
        *self = Self::default();
        for relationship in all {
            self.insert(relationship);
        }
        for id in ignored_only {
            self.set_ignored(id, true);
        }
        self.rebuild();
    }

    /// Returns whether the set changed. A new ignore is refused at the bound.
    fn set_ignored(&mut self, id: Snowflake, ignored: bool) -> bool {
        if ignored {
            if self.ignored.len() >= MAX_RELATIONSHIPS && !self.ignored.contains(&id) {
                return false;
            }
            let added = self.ignored.insert(id);
            if added {
                self.bytes += IGNORED_COST;
            }
            added
        } else {
            let removed = self.ignored.remove(&id);
            if removed {
                self.bytes -= IGNORED_COST;
            }
            removed
        }
    }

    fn insert(&mut self, relationship: Relationship) -> bool {
        let id = relationship.user.id;
        if self.map.len() >= MAX_RELATIONSHIPS && !self.map.contains_key(&id) {
            return false;
        }
        // An add never clears an ignore (the payload may simply omit the flag);
        // only an explicit update or a new READY does.
        if relationship.ignored {
            self.set_ignored(id, true);
        }
        self.bytes += cost(&relationship);
        if let Some(old) = self.map.insert(id, relationship) {
            self.bytes -= cost(&old);
        }
        true
    }

    /// Returns whether anything changed.
    pub(super) fn add(&mut self, relationship: Relationship) -> bool {
        let same = self.map.get(&relationship.user.id) == Some(&relationship)
            && (!relationship.ignored || self.ignored.contains(&relationship.user.id));
        if same || !self.insert(relationship) {
            return false;
        }
        self.rebuild();
        true
    }

    /// A stale update for a relationship that is gone changes nothing for its
    /// kind; the ignore flag applies whether or not a kept kind is held, and a
    /// payload that omits it leaves it alone.
    pub(super) fn update(&mut self, update: RelationshipUpdate) -> bool {
        let mut changed = false;
        if let Some(kind) = update.kind
            && let Some(held) = self.map.get_mut(&update.id)
            && held.kind != kind
        {
            held.kind = kind;
            changed = true;
        }
        if let Some(ignored) = update.ignored {
            if let Some(held) = self.map.get_mut(&update.id) {
                held.ignored = ignored;
            }
            changed |= self.set_ignored(update.id, ignored);
        }
        if changed {
            self.rebuild();
        }
        changed
    }

    /// Removes the relationship. An ignore on a kept kind survives (it is
    /// separate from friendship and blocking, so an unfriended user stays
    /// hidden rather than leaking back); an entry that existed only as an
    /// ignore is gone with it.
    pub(super) fn remove(&mut self, id: Snowflake) -> bool {
        let Some(old) = self.map.remove(&id) else {
            let removed = self.set_ignored(id, false);
            if removed {
                self.rebuild();
            }
            return removed;
        };
        self.bytes -= cost(&old);
        self.rebuild();
        true
    }

    pub(super) fn suppression(&self, id: Snowflake) -> Option<Suppression> {
        self.suppressed.get(&id).copied()
    }

    pub(super) fn suppressed(&self) -> Arc<BTreeMap<Snowflake, Suppression>> {
        Arc::clone(&self.suppressed)
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
        self.map.values().map(cost).sum::<usize>() + self.ignored.len() * IGNORED_COST
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
        // An ignore outlives its kept relationship (see `remove`), so the view
        // is built from the set first and blocking is laid over it.
        let mut suppressed: BTreeMap<_, _> = self
            .ignored
            .iter()
            .map(|id| (*id, Suppression::Ignored))
            .collect();
        for relationship in self.map.values() {
            if relationship.kind == RelationshipKind::Blocked {
                suppressed.insert(relationship.user.id, Suppression::Blocked);
            }
        }
        self.suppressed = Arc::new(suppressed);
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
