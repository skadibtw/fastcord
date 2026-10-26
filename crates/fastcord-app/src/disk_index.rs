//! In-memory byte ledger of the on-disk image caches (#168). A directory is
//! scanned once per process; afterwards inserts, touches and evictions update
//! the ledger, so a cache miss costs no directory walk. The mutex guards only
//! the ledger: callers delete the returned victims and do all other file I/O
//! outside it.
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use parking_lot::Mutex;

use crate::attachments::AttachmentError;

/// Every file is charged at least this much, as the real cost of a tiny file.
pub(crate) const MIN_CHARGE: u64 = 4096;

#[derive(Default)]
struct Index {
    total: u64,
    tick: u64,
    /// path -> (recency tick, charged bytes, evictable)
    files: HashMap<PathBuf, (u64, u64, bool)>,
    /// recency tick -> evictable path, oldest first
    order: BTreeMap<u64, PathBuf>,
}

impl Index {
    fn remove(&mut self, path: &Path) {
        if let Some((tick, charged, _)) = self.files.remove(path) {
            self.total = self.total.saturating_sub(charged);
            self.order.remove(&tick);
        }
    }

    fn insert(&mut self, path: PathBuf, size: u64, evictable: bool) {
        self.remove(&path);
        self.tick += 1;
        let charged = size.max(MIN_CHARGE);
        self.total = self.total.saturating_add(charged);
        if evictable {
            self.order.insert(self.tick, path.clone());
        }
        self.files.insert(path, (self.tick, charged, evictable));
    }

    /// Takes the least recently used files out of the ledger until at most
    /// `limit` bytes remain. `Err` if protected files alone exceed it.
    fn evict(&mut self, limit: u64, victims: &mut Vec<PathBuf>) -> bool {
        while self.total > limit {
            let Some((_, path)) = self.order.pop_first() else {
                return false;
            };
            if let Some((_, charged, _)) = self.files.remove(&path) {
                self.total = self.total.saturating_sub(charged);
            }
            victims.push(path);
        }
        true
    }
}

/// One ledger per cache base directory (the parent of the account directories).
static INDEXES: LazyLock<Mutex<HashMap<PathBuf, Index>>> = LazyLock::new(Default::default);

fn base_of(file: &Path) -> PathBuf {
    file.parent()
        .and_then(Path::parent)
        .unwrap_or(file)
        .to_owned()
}

pub(crate) fn is_temporary(path: &Path) -> bool {
    path.extension().is_some_and(|extension| extension == "tmp")
}

/// Scans every account directory under `base` once per process.
pub(crate) fn ensure(base: &Path) -> Result<(), AttachmentError> {
    if INDEXES.lock().contains_key(base) {
        return Ok(());
    }
    let unavailable = |_| AttachmentError::CacheUnavailable;
    let mut found = Vec::new();
    for account in fs::read_dir(base).map_err(unavailable)? {
        let account = account.map_err(unavailable)?;
        if !account.file_type().map_err(unavailable)?.is_dir() {
            continue;
        }
        for entry in fs::read_dir(account.path()).map_err(unavailable)? {
            let entry = entry.map_err(unavailable)?;
            let metadata = entry.metadata().map_err(unavailable)?;
            if metadata.is_file() {
                let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
                found.push((modified, entry.path(), metadata.len()));
            }
        }
    }
    found.sort_by_key(|file| file.0);
    let mut index = Index::default();
    for (_, path, size) in found {
        let evictable = !is_temporary(&path);
        index.insert(path, size, evictable);
    }
    INDEXES.lock().entry(base.to_owned()).or_insert(index);
    Ok(())
}

/// Records `file` (`size` bytes, newest) replacing `replacing` if given, and
/// evicts older files down to `limit`. Returns the files to delete, and
/// whether the budget holds. A `.tmp` file is a protected partial download.
/// On a broken budget the new file is left out of the ledger again.
pub(crate) fn record(
    file: &Path,
    size: u64,
    replacing: Option<&Path>,
    limit: u64,
) -> (Vec<PathBuf>, bool) {
    let mut victims = Vec::new();
    let mut indexes = INDEXES.lock();
    let Some(index) = indexes.get_mut(&base_of(file)) else {
        return (victims, true);
    };
    if let Some(replacing) = replacing {
        index.remove(replacing);
    }
    index.insert(file.to_owned(), size, !is_temporary(file));
    // The file just recorded is never its own victim.
    let (tick, _, evictable) = index.files[file];
    if evictable {
        index.order.remove(&tick);
    }
    let fits = index.evict(limit, &mut victims);
    if evictable {
        index.order.insert(tick, file.to_owned());
    }
    if !fits {
        index.remove(file);
    }
    (victims, fits)
}

/// Marks `file` most recently used.
pub(crate) fn touch(file: &Path) {
    let mut indexes = INDEXES.lock();
    let Some(index) = indexes.get_mut(&base_of(file)) else {
        return;
    };
    let Some(&(tick, charged, evictable)) = index.files.get(file) else {
        return;
    };
    index.tick += 1;
    index
        .files
        .insert(file.to_owned(), (index.tick, charged, evictable));
    if evictable {
        index.order.remove(&tick);
        index.order.insert(index.tick, file.to_owned());
    }
}

pub(crate) fn forget(file: &Path) {
    if let Some(index) = INDEXES.lock().get_mut(&base_of(file)) {
        index.remove(file);
    }
}

/// Forgets every file under `dir` (the directory is being removed).
pub(crate) fn forget_dir(dir: &Path) {
    if let Some(index) = INDEXES.lock().get_mut(dir.parent().unwrap_or(dir)) {
        let gone: Vec<_> = index
            .files
            .keys()
            .filter(|path| path.starts_with(dir))
            .cloned()
            .collect();
        for path in gone {
            index.remove(&path);
        }
    }
}

/// Deletes evicted files; a file that is already gone is fine.
pub(crate) fn delete(victims: Vec<PathBuf>) {
    for path in victims {
        let _ = fs::remove_file(path);
    }
}

#[cfg(test)]
pub(crate) fn total(base: &Path) -> u64 {
    INDEXES.lock().get(base).map_or(0, |index| index.total)
}
