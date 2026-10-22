//! Embedded Activities running in voice channels (issue #102), read-only: at
//! most [`MAX_PER_CHANNEL`] per channel and [`MAX_PER_GUILD`] per guild, names
//! capped by the decoder. Updates are idempotent per (channel, application):
//! a replay changes nothing and an end of an unknown activity is ignored.

use std::mem::size_of;

use fastcord_model::{ActivityUpdate, Snowflake};

use super::{Changes, GuildEntry, Store};

pub const MAX_PER_CHANNEL: usize = 3;
const MAX_PER_GUILD: usize = 32;

pub(super) fn bytes(activity: &ActivityUpdate) -> usize {
    size_of::<ActivityUpdate>() + activity.name.capacity()
}

impl GuildEntry {
    /// Names of the Activities running in a channel (empty when unnamed).
    pub fn activities(&self, channel: Snowflake) -> impl Iterator<Item = &str> {
        self.activities
            .iter()
            .filter(move |a| a.channel_id == channel)
            .map(|a| a.name.as_str())
    }
}

impl Store {
    pub(super) fn activity_event(&mut self, update: ActivityUpdate, changes: &mut Changes) {
        let guild = update.guild_id;
        let changed = self.with_guild(guild, |entry| {
            let at = entry.activities.iter().position(|a| {
                a.channel_id == update.channel_id && a.application_id == update.application_id
            });
            let in_channel = entry
                .activities
                .iter()
                .filter(|a| a.channel_id == update.channel_id)
                .count();
            let changed = match (at, update.running) {
                (Some(at), true) => {
                    let same = entry.activities[at] == update;
                    entry.activities[at] = update;
                    !same
                }
                (None, true)
                    if in_channel < MAX_PER_CHANNEL && entry.activities.len() < MAX_PER_GUILD =>
                {
                    entry.activities.push(update);
                    true
                }
                (Some(at), false) => {
                    entry.activities.remove(at);
                    entry.activities.shrink_to_fit();
                    true
                }
                _ => false,
            };
            entry.recompute_meta();
            changed
        });
        if changed == Some(true) {
            changes.guilds.insert(guild);
            changes.voice.insert(guild);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{Dispatch, GatewayEvent, decode_fixture};
    use serde_json::json;

    const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
    const GUILD: u64 = 41_771_983_423_143_937;
    const CHANNEL: u64 = 41_771_983_423_143_939;

    fn store() -> Store {
        let mut store = Store::new();
        let Dispatch::Ready(ready) = decode_fixture("READY", READY) else {
            panic!("READY did not decode");
        };
        store.apply(GatewayEvent::Dispatch {
            sequence: 1,
            event: Dispatch::Ready(ready),
        });
        store
    }

    fn send(store: &mut Store, name: &str, raw: serde_json::Value) -> Changes {
        let changes = store.apply(GatewayEvent::Dispatch {
            sequence: 2,
            event: decode_fixture(name, &raw.to_string()),
        });
        assert_eq!(store.bytes(), store.recount());
        changes
    }

    fn update(app: u64, name: &str, users: usize) -> serde_json::Value {
        let users: Vec<_> = (0..users)
            .map(|n| json!({"user_id": n.to_string()}))
            .collect();
        json!({"guild_id": GUILD.to_string(), "channel_id": CHANNEL.to_string(),
            "embedded_activity": {"application_id": app.to_string(), "name": name},
            "participants": users})
    }

    fn names(store: &Store) -> Vec<String> {
        let guild = store.guild(Snowflake(GUILD)).unwrap();
        guild
            .activities(Snowflake(CHANNEL))
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn both_wire_shapes_decode_and_names_are_bounded() {
        let v2 = decode_fixture(
            "EMBEDDED_ACTIVITY_UPDATE_V2",
            r#"{"guild_id":"1","location":{"channel_id":"2"},"embedded_activity":
                {"application_id":"3"},"connections":[{"user_id":"4"}]}"#,
        );
        let Dispatch::EmbeddedActivity(a) = v2 else {
            panic!("V2 did not decode");
        };
        assert_eq!(
            (a.channel_id, a.application_id),
            (Snowflake(2), Snowflake(3))
        );
        assert!(a.running && a.name.is_empty());
        let long = update(5, &"n\u{7}".repeat(500), 0).to_string();
        let Dispatch::EmbeddedActivity(a) = decode_fixture("EMBEDDED_ACTIVITY_UPDATE", &long)
        else {
            panic!("did not decode");
        };
        assert!(!a.running, "no participants ends it");
        assert_eq!(
            a.name.chars().count(),
            fastcord_model::MAX_ACTIVITY_NAME_CHARS
        );
        assert!(!a.name.contains('\u{7}'));
    }

    #[test]
    fn updates_replay_settle_and_stay_bounded() {
        let mut store = store();
        // An end of an unknown activity changes nothing.
        assert!(send(&mut store, "EMBEDDED_ACTIVITY_UPDATE", update(9, "x", 0)).is_empty());
        assert!(
            !send(
                &mut store,
                "EMBEDDED_ACTIVITY_UPDATE",
                update(1, "Watch", 2)
            )
            .is_empty()
        );
        assert_eq!(names(&store), ["Watch"]);
        // A replay is a no-op; a rename replaces in place.
        assert!(
            send(
                &mut store,
                "EMBEDDED_ACTIVITY_UPDATE",
                update(1, "Watch", 2)
            )
            .is_empty()
        );
        send(
            &mut store,
            "EMBEDDED_ACTIVITY_UPDATE_V2",
            update(1, "Watch 2", 1),
        );
        assert_eq!(names(&store), ["Watch 2"]);
        // At most MAX_PER_CHANNEL per channel.
        for app in 2..10 {
            send(&mut store, "EMBEDDED_ACTIVITY_UPDATE", update(app, "a", 1));
        }
        assert_eq!(names(&store).len(), MAX_PER_CHANNEL);
        // The end removes it; a repeated end (stale replay) changes nothing.
        assert!(!send(&mut store, "EMBEDDED_ACTIVITY_UPDATE", update(1, "", 0)).is_empty());
        assert!(!names(&store).contains(&"Watch 2".to_owned()));
        assert!(send(&mut store, "EMBEDDED_ACTIVITY_UPDATE", update(1, "", 0)).is_empty());
    }
}
