//! The non-secret index of saved accounts (SPEC §16, issue #107): user id,
//! display name and avatar hash, kept in a small file next to the other app
//! settings so the account list can show names without reading any token.
//!
//! The tokens stay in the native credential store, one entry per user id; the
//! store is the authority on which accounts exist. The index only labels them:
//! it is reconciled against the store's account list on every start, so an
//! account saved before the index existed (or an index row whose token was
//! removed elsewhere) needs no migration step. A missing or damaged file is an
//! empty index.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fastcord_model::{Snowflake, User};

const MAX_FILE_BYTES: u64 = 16 * 1024;
/// Saved accounts listed; more are kept in the store but shown by id only.
pub const MAX_ACCOUNTS: usize = 32;
const MAX_NAME_CHARS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: Snowflake,
    /// Empty until the account has signed in once since the index existed.
    pub name: String,
    pub avatar: Option<String>,
}

impl Entry {
    pub fn label(&self) -> String {
        if self.name.is_empty() {
            crate::i18n::trf("Account {}", &[&self.id])
        } else {
            self.name.clone()
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Index(Vec<Entry>);

fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME_CHARS)
        .collect()
}

impl Index {
    pub fn entries(&self) -> &[Entry] {
        &self.0
    }

    pub fn get(&self, id: Snowflake) -> Option<&Entry> {
        self.0.iter().find(|entry| entry.id == id)
    }

    /// Records (or refreshes) the label of an account that just signed in.
    pub fn upsert(&mut self, user: &User) {
        let entry = Entry {
            id: user.id,
            name: clean(user.display_name()),
            avatar: user
                .avatar
                .as_deref()
                .map(clean)
                .filter(|hash| !hash.is_empty()),
        };
        if let Some(slot) = self.0.iter_mut().find(|e| e.id == user.id) {
            *slot = entry;
        } else if self.0.len() < MAX_ACCOUNTS {
            self.0.push(entry);
        }
    }

    pub fn remove(&mut self, id: Snowflake) {
        self.0.retain(|entry| entry.id != id);
    }

    /// Makes the index list exactly the accounts the credential store holds,
    /// in the store's order; known labels are kept.
    pub fn reconcile(&mut self, stored: &[Snowflake]) {
        let old = std::mem::take(&mut self.0);
        self.0 = stored
            .iter()
            .take(MAX_ACCOUNTS)
            .map(|id| {
                old.iter().find(|e| e.id == *id).cloned().unwrap_or(Entry {
                    id: *id,
                    name: String::new(),
                    avatar: None,
                })
            })
            .collect();
    }

    /// One line per account: `id`, avatar hash, name, tab separated.
    fn encode(&self) -> String {
        self.0
            .iter()
            .map(|e| {
                format!(
                    "{}\t{}\t{}\n",
                    e.id,
                    e.avatar.as_deref().unwrap_or(""),
                    e.name.replace('\t', " ")
                )
            })
            .collect()
    }

    fn parse(text: &str) -> Self {
        let mut index = Self::default();
        for line in text.lines().take(MAX_ACCOUNTS) {
            let mut parts = line.splitn(3, '\t');
            let (Some(id), Some(avatar), Some(name)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let Ok(id) = id.parse::<Snowflake>() else {
                continue;
            };
            if id.0 != 0 && index.get(id).is_none() {
                index.0.push(Entry {
                    id,
                    name: clean(name),
                    avatar: Some(clean(avatar)).filter(|a| !a.is_empty()),
                });
            }
        }
        index
    }
}

fn index_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("APPDATA").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .join("Library/Application Support");
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(
        root.join(crate::audio_devices::app_dir_name())
            .join("accounts.tsv"),
    )
}

fn load_from(path: &Path) -> Index {
    if !fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_FILE_BYTES) {
        return Index::default();
    }
    fs::read_to_string(path).map_or_else(|_| Index::default(), |text| Index::parse(&text))
}

fn save_to(path: &Path, index: &Index) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("index parent unavailable"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("tsv.tmp");
    fs::write(&temporary, index.encode())?;
    fs::rename(temporary, path)
}

/// Blocking; call from a worker.
pub fn load() -> Index {
    index_path().map_or_else(Index::default, |path| load_from(&path))
}

/// Blocking; call from a worker. A failure only loses labels, never a login.
pub fn save(index: &Index) -> io::Result<()> {
    let path = index_path().ok_or_else(|| io::Error::other("index path unavailable"))?;
    save_to(&path, index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u64, name: &str, avatar: Option<&str>) -> User {
        User {
            id: Snowflake(id),
            username: name.to_owned(),
            global_name: None,
            avatar: avatar.map(str::to_owned),
            bot: false,
        }
    }

    #[test]
    fn the_index_round_trips_through_a_file() {
        let path =
            std::env::temp_dir().join(format!("fastcord-accounts-{}.tsv", std::process::id()));
        let mut index = Index::default();
        index.upsert(&user(10, "alt one", Some("abc123")));
        index.upsert(&user(20, "tab\tname\n", None));
        save_to(&path, &index).unwrap();
        let loaded = load_from(&path);
        assert_eq!(loaded, index);
        assert_eq!(
            loaded.get(Snowflake(10)).unwrap().avatar.as_deref(),
            Some("abc123")
        );
        assert_eq!(loaded.get(Snowflake(20)).unwrap().name, "tabname");
        fs::remove_file(&path).unwrap();
        assert_eq!(load_from(&path), Index::default());
    }

    #[test]
    fn an_account_saved_before_the_index_existed_is_migrated_by_reconcile() {
        // The store holds two tokens under their user ids; the index is empty.
        let mut index = Index::default();
        index.reconcile(&[Snowflake(1), Snowflake(2)]);
        assert_eq!(index.entries().len(), 2);
        assert_eq!(index.get(Snowflake(1)).unwrap().label(), "Account 1");
        // Signing in labels it; a later reconcile keeps the label, drops a
        // removed token's row and keeps the store's order.
        index.upsert(&user(2, "alt", None));
        index.reconcile(&[Snowflake(2), Snowflake(3)]);
        let ids: Vec<u64> = index.entries().iter().map(|e| e.id.0).collect();
        assert_eq!(ids, [2, 3]);
        assert_eq!(index.get(Snowflake(2)).unwrap().label(), "alt");
    }

    #[test]
    fn damaged_lines_are_skipped_and_the_index_is_bounded() {
        let parsed = Index::parse("0\t\tzero\nnot-an-id\t\tx\n5\t\tfive\n5\t\tdup\nshort\n");
        assert_eq!(parsed.entries().len(), 1);
        assert_eq!(parsed.entries()[0].name, "five");
        let mut index = Index::default();
        for id in 1..=(MAX_ACCOUNTS as u64 + 5) {
            index.upsert(&user(id, "x", None));
        }
        assert_eq!(index.entries().len(), MAX_ACCOUNTS);
        let long = "n".repeat(500);
        index.upsert(&user(1, &long, None));
        assert_eq!(
            index.get(Snowflake(1)).unwrap().name.chars().count(),
            MAX_NAME_CHARS
        );
    }

    #[test]
    fn no_token_is_ever_part_of_the_index() {
        let mut index = Index::default();
        index.upsert(&user(7, "alt", Some("hash")));
        assert_eq!(index.encode(), "7\thash\talt\n");
    }
}
