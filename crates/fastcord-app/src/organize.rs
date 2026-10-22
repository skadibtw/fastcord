//! Local conversation organization (issue #106): which server folders are
//! open, which categories are folded and which servers hide their muted
//! channels. These are per-account choices kept in the settings file next to
//! the other preferences and never sent to Discord. The reducer reads the
//! current account's copy from one process-wide slot, like `chat_prefs`, so no
//! plumbing is needed beyond a nudge when it changes.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, RwLock};

use fastcord_discord::state::navigation::Layout;
use fastcord_model::Snowflake;
use serde_json::{Value, json};

/// Accounts remembered, and entries per set per account.
const MAX_ACCOUNTS: usize = 16;
const MAX_ENTRIES: usize = 1024;

/// One click on the lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Category(Snowflake),
    Folder(u64),
    HideMuted(Snowflake),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Local {
    accounts: BTreeMap<u64, Arc<Layout>>,
}

fn flip<T: std::hash::Hash + Eq>(set: &mut HashSet<T>, value: T) {
    if !set.remove(&value) && set.len() < MAX_ENTRIES {
        set.insert(value);
    }
}

impl Local {
    /// The layout of one account (empty when it has none yet).
    pub fn layout(&self, account: u64) -> Arc<Layout> {
        self.accounts.get(&account).cloned().unwrap_or_default()
    }

    pub fn apply(&mut self, account: u64, event: Event) {
        if !self.accounts.contains_key(&account) && self.accounts.len() >= MAX_ACCOUNTS {
            return;
        }
        let layout = Arc::make_mut(self.accounts.entry(account).or_default());
        match event {
            Event::Category(id) => flip(&mut layout.collapsed_categories, id),
            Event::Folder(id) => flip(&mut layout.open_folders, id),
            Event::HideMuted(id) => flip(&mut layout.hide_muted, id),
        }
    }

    pub fn from_json(value: &Value) -> Self {
        fn ids(value: &Value, key: &str) -> Vec<u64> {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_u64)
                        .take(MAX_ENTRIES)
                        .collect()
                })
                .unwrap_or_default()
        }
        let accounts = value
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(account, entry)| {
                let layout = Layout {
                    collapsed_categories: ids(entry, "categories")
                        .into_iter()
                        .map(Snowflake)
                        .collect(),
                    open_folders: ids(entry, "folders").into_iter().collect(),
                    hide_muted: ids(entry, "hide_muted")
                        .into_iter()
                        .map(Snowflake)
                        .collect(),
                };
                Some((account.parse().ok()?, Arc::new(layout)))
            })
            .take(MAX_ACCOUNTS)
            .collect();
        Self { accounts }
    }

    pub fn to_json(&self) -> Value {
        fn sorted<T: Copy>(set: &HashSet<T>, key: impl Fn(T) -> u64) -> Vec<u64> {
            let mut ids: Vec<u64> = set.iter().map(|id| key(*id)).collect();
            ids.sort_unstable();
            ids
        }
        Value::Object(
            self.accounts
                .iter()
                .map(|(account, layout)| {
                    (
                        account.to_string(),
                        json!({
                            "categories": sorted(&layout.collapsed_categories, |id| id.0),
                            "folders": sorted(&layout.open_folders, |id| id),
                            "hide_muted": sorted(&layout.hide_muted, |id| id.0),
                        }),
                    )
                })
                .collect(),
        )
    }
}

static CURRENT: RwLock<Option<Local>> = RwLock::new(None);

/// Makes `local` the choices in force.
pub fn publish(local: &Local) {
    match CURRENT.write() {
        Ok(mut current) => *current = Some(local.clone()),
        Err(poisoned) => *poisoned.into_inner() = Some(local.clone()),
    }
}

/// The layout in force for `account`.
pub fn current(account: u64) -> Arc<Layout> {
    CURRENT
        .read()
        .map_or_else(
            |poisoned| poisoned.into_inner().as_ref().map(|l| l.layout(account)),
            |current| current.as_ref().map(|l| l.layout(account)),
        )
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicks_flip_per_account_and_round_trip() {
        let mut local = Local::default();
        local.apply(1, Event::Category(Snowflake(10)));
        local.apply(1, Event::Folder(7));
        local.apply(2, Event::HideMuted(Snowflake(5)));
        assert!(
            local
                .layout(1)
                .collapsed_categories
                .contains(&Snowflake(10))
        );
        assert!(local.layout(1).hide_muted.is_empty());
        local.apply(1, Event::Category(Snowflake(10)));
        assert!(local.layout(1).collapsed_categories.is_empty());
        local.apply(1, Event::Category(Snowflake(11)));
        assert_eq!(Local::from_json(&local.to_json()), local);
        assert_eq!(Local::from_json(&json!("junk")), Local::default());
        assert_eq!(
            Local::from_json(&json!({"x": {}, "3": {"folders": [1, "a"]}}))
                .layout(3)
                .open_folders
                .len(),
            1
        );
    }

    #[test]
    fn sets_and_accounts_are_bounded() {
        let mut local = Local::default();
        for n in 0..(MAX_ENTRIES as u64 + 10) {
            local.apply(1, Event::Folder(n));
        }
        assert_eq!(local.layout(1).open_folders.len(), MAX_ENTRIES);
        for account in 0..40 {
            local.apply(account, Event::Folder(1));
        }
        assert_eq!(local.accounts.len(), MAX_ACCOUNTS);
    }
}
