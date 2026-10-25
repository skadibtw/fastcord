use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use fastcord_model::Snowflake;

use crate::gateway::UserVolume;

const MAX_FILE_BYTES: u64 = 32 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_ACCOUNT_ENTRIES: usize = 256;
static FILE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Saved,
    Evicted,
    Failed,
}

impl Status {
    pub fn notice(self) -> Option<&'static str> {
        match self {
            Self::Saved => None,
            Self::Evicted => Some(
                "Voice volume settings keep the 256 most recently changed people per account (512 total). Older settings were removed.",
            ),
            Self::Failed => Some(
                "Could not save voice volumes. Change a volume to retry before closing fastcord.",
            ),
        }
    }
}

#[derive(Default)]
struct Pending {
    volumes: Vec<UserVolume>,
    flush: bool,
    stop: bool,
    evicted: bool,
    changed: bool,
}

/// One account's writer. Changes coalesce in at most 256 small rows; no file
/// I/O runs in a widget event. Release requests an early flush, but is optional.
pub struct Writer {
    pending: Arc<(Mutex<Pending>, Condvar)>,
    thread: Option<std::thread::JoinHandle<Status>>,
}

impl Writer {
    pub fn start(account: Snowflake) -> (Self, impl iced::futures::Stream<Item = Status>) {
        let (writer, receiver) = Self::at(settings_path(), account);
        let events = iced::futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.changed().await.ok()?;
            let status = *receiver.borrow_and_update();
            Some((status, receiver))
        });
        (writer, events)
    }

    pub(crate) fn at(
        path: Option<PathBuf>,
        account: Snowflake,
    ) -> (Self, tokio::sync::watch::Receiver<Status>) {
        let pending = Arc::new((Mutex::new(Pending::default()), Condvar::new()));
        let worker = pending.clone();
        let (status, receiver) = tokio::sync::watch::channel(Status::Saved);
        let thread = std::thread::Builder::new()
            .name("voice-settings".into())
            .spawn(move || {
                let (lock, wake) = &*worker;
                let mut last = Status::Saved;
                loop {
                    let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                    while pending.volumes.is_empty() && !pending.stop {
                        pending = wake.wait(pending).unwrap_or_else(|e| e.into_inner());
                    }
                    if !pending.stop && !pending.flush {
                        // A bounded delay from the first change also saves a long
                        // drag periodically, even if no release ever arrives.
                        pending = wake
                            .wait_timeout_while(pending, Duration::from_millis(500), |p| {
                                !p.flush && !p.stop
                            })
                            .unwrap_or_else(|e| e.into_inner())
                            .0;
                    }
                    let rows = std::mem::take(&mut pending.volumes);
                    let evicted = pending.evicted;
                    let stop = pending.stop;
                    pending.flush = false;
                    pending.changed = false;
                    drop(pending);
                    if !rows.is_empty() {
                        let result = path
                            .as_deref()
                            .ok_or_else(|| io::Error::other("settings path unavailable"))
                            .and_then(|path| save_many_to(path, account, &rows));
                        last = match result {
                            Ok(trimmed) if trimmed || evicted => Status::Evicted,
                            Ok(_) => Status::Saved,
                            Err(_) => {
                                // Keep failed changes for the next explicit change
                                // or final flush; do not spin on an unwritable file.
                                let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                                for row in rows.into_iter().rev() {
                                    if !pending.volumes.iter().any(|v| v.user_id == row.user_id)
                                        && pending.volumes.len() < MAX_ACCOUNT_ENTRIES
                                    {
                                        pending.volumes.insert(0, row);
                                    }
                                }
                                Status::Failed
                            }
                        };
                        status.send_replace(last);
                    }
                    if stop {
                        return last;
                    }
                    // A failed write waits for a new change or shutdown, rather
                    // than immediately attempting the retained rows again.
                    if last == Status::Failed {
                        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
                        while !pending.changed && !pending.flush && !pending.stop {
                            pending = wake.wait(pending).unwrap_or_else(|e| e.into_inner());
                        }
                    }
                }
            })
            .ok();
        (Self { pending, thread }, receiver)
    }

    pub fn set(&self, user_id: Snowflake, percent: u16) -> bool {
        if self.thread.is_none() {
            return false;
        }
        let (lock, wake) = &*self.pending;
        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = pending.volumes.iter().position(|v| v.user_id == user_id) {
            pending.volumes.remove(index);
        } else if pending.volumes.len() == MAX_ACCOUNT_ENTRIES {
            pending.volumes.remove(0);
            pending.evicted = true;
        }
        pending.volumes.push(UserVolume {
            user_id,
            percent: percent.min(200),
        });
        // Also wakes a failed writer for one user-requested retry.
        pending.changed = true;
        wake.notify_one();
        true
    }

    pub fn flush(&self) {
        let (lock, wake) = &*self.pending;
        lock.lock().unwrap_or_else(|e| e.into_inner()).flush = true;
        wake.notify_one();
    }

    pub fn finish(mut self) -> Status {
        self.stop()
    }

    fn stop(&mut self) -> Status {
        let (lock, wake) = &*self.pending;
        lock.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
        wake.notify_one();
        self.thread.take().map_or(Status::Failed, |thread| {
            thread.join().unwrap_or(Status::Failed)
        })
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.stop();
        }
    }
}

fn settings_path() -> Option<PathBuf> {
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
            .join("voice-settings.csv"),
    )
}

fn parse(contents: &str) -> Vec<(Snowflake, UserVolume)> {
    let mut entries: Vec<(Snowflake, UserVolume)> = Vec::new();
    for line in contents.lines().take(MAX_ENTRIES) {
        let mut fields = line.split(',');
        let (Some(account), Some(user), Some(percent), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let (Ok(account), Ok(user), Ok(percent)) = (
            account.parse::<u64>(),
            user.parse::<u64>(),
            percent.parse::<u16>(),
        ) else {
            continue;
        };
        if percent <= 200 {
            let owner = Snowflake(account);
            if let Some((_, existing)) = entries.iter_mut().find(|(existing_owner, volume)| {
                *existing_owner == owner && volume.user_id.0 == user
            }) {
                existing.percent = percent;
            } else {
                entries.push((
                    owner,
                    UserVolume {
                        user_id: Snowflake(user),
                        percent,
                    },
                ));
            }
        }
    }
    entries
}
fn encode(entries: &[(Snowflake, UserVolume)]) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    for (account, volume) in entries.iter().take(MAX_ENTRIES) {
        let _ = writeln!(
            output,
            "{},{},{}",
            account.0,
            volume.user_id.0,
            volume.percent.min(200)
        );
    }
    output
}

/// The account's rows, newest last. When more than the per-account bound exist, the
/// newest are kept: those are the ones `save_to` appended most recently.
fn for_account(entries: Vec<(Snowflake, UserVolume)>, account: Snowflake) -> Vec<UserVolume> {
    let mut rows: Vec<UserVolume> = entries
        .into_iter()
        .filter_map(|(owner, volume)| (owner == account).then_some(volume))
        .collect();
    let excess = rows.len().saturating_sub(MAX_ACCOUNT_ENTRIES);
    rows.drain(..excess);
    rows
}

pub fn load(account: Snowflake) -> Vec<UserVolume> {
    let Some(path) = settings_path() else {
        return Vec::new();
    };
    load_from(&path, account)
}

pub(crate) fn load_from(path: &std::path::Path, account: Snowflake) -> Vec<UserVolume> {
    let Ok(metadata) = fs::metadata(path) else {
        return Vec::new();
    };
    if metadata.len() > MAX_FILE_BYTES {
        return Vec::new();
    }
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    for_account(parse(&contents), account)
}

#[cfg(test)]
fn save_to(
    path: &std::path::Path,
    account: Snowflake,
    user: Snowflake,
    percent: u16,
) -> io::Result<()> {
    save_many_to(
        path,
        account,
        &[UserVolume {
            user_id: user,
            percent,
        }],
    )
    .map(|_| ())
}

fn save_many_to(
    path: &std::path::Path,
    account: Snowflake,
    rows: &[UserVolume],
) -> io::Result<bool> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut entries = if fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_FILE_BYTES) {
        fs::read_to_string(path).map_or_else(|_| Vec::new(), |text| parse(&text))
    } else {
        Vec::new()
    };
    for row in rows {
        entries.retain(|(owner, volume)| *owner != account || volume.user_id != row.user_id);
        entries.push((
            account,
            UserVolume {
                user_id: row.user_id,
                percent: row.percent.min(200),
            },
        ));
    }
    let mut kept = 0;
    let before = entries.len();
    entries.reverse();
    entries.retain(|(owner, _)| {
        if *owner != account {
            return true;
        }
        kept += 1;
        kept <= MAX_ACCOUNT_ENTRIES
    });
    entries.reverse();
    let evicted = before != entries.len() || entries.len() > MAX_ENTRIES;
    let excess = entries.len().saturating_sub(MAX_ENTRIES);
    entries.drain(..excess);
    let output = encode(&entries);
    if output.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::other("voice settings exceed the file limit"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("settings parent unavailable"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("csv.tmp");
    fs::write(&temporary, output)?;
    fs::rename(temporary, path)?;
    Ok(evicted)
}

/// Forgets this account's volume rows (logout or removing the saved login).
pub fn purge(account: Snowflake) -> io::Result<()> {
    let path = settings_path().ok_or_else(|| io::Error::other("settings path unavailable"))?;
    purge_from(&path, account)
}

fn purge_from(path: &std::path::Path, account: Snowflake) -> io::Result<()> {
    let _guard = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_FILE_BYTES) {
        return Ok(());
    }
    let mut entries = parse(&fs::read_to_string(path)?);
    let before = entries.len();
    entries.retain(|(owner, _)| *owner != account);
    if entries.len() == before {
        return Ok(());
    }
    if entries.is_empty() {
        return fs::remove_file(path);
    }
    let temporary = path.with_extension("csv.tmp");
    fs::write(&temporary, encode(&entries))?;
    fs::rename(temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "fastcord-volume-writer-{}-{id}",
                std::process::id()
            ))
            .join("voice-settings.csv")
    }

    #[test]
    fn finishing_writer_flushes_changes_without_slider_release() {
        let path = test_path();
        let (writer, _) = Writer::at(Some(path.clone()), Snowflake(1));
        writer.set(Snowflake(2), 6);
        writer.set(Snowflake(3), 67);
        writer.set(Snowflake(2), 62);
        assert_eq!(writer.finish(), Status::Saved);
        assert_eq!(
            load_from(&path, Snowflake(1)),
            [
                UserVolume {
                    user_id: Snowflake(3),
                    percent: 67
                },
                UserVolume {
                    user_id: Snowflake(2),
                    percent: 62
                },
            ]
        );
        assert!(load_from(&path, Snowflake(4)).is_empty());
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn change_is_saved_without_release_and_failures_can_be_retried() {
        let path = test_path();
        // A file where the parent directory must be makes saving fail on all OSes.
        fs::write(path.parent().unwrap(), b"blocked").unwrap();
        let (writer, mut status) = Writer::at(Some(path.clone()), Snowflake(1));
        writer.set(Snowflake(2), 6);
        tokio::time::timeout(Duration::from_secs(10), status.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*status.borrow_and_update(), Status::Failed);
        fs::remove_file(path.parent().unwrap()).unwrap();
        writer.set(Snowflake(3), 67);
        tokio::time::timeout(Duration::from_secs(10), status.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*status.borrow_and_update(), Status::Saved);
        assert_eq!(writer.finish(), Status::Saved);
        assert_eq!(load_from(&path, Snowflake(1)).len(), 2);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn recently_changed_rows_survive_the_account_limit() {
        let path = test_path();
        let rows: Vec<_> = (1..=256)
            .map(|id| UserVolume {
                user_id: Snowflake(id),
                percent: 50,
            })
            .collect();
        save_many_to(&path, Snowflake(1), &rows).unwrap();
        save_to(&path, Snowflake(1), Snowflake(1), 6).unwrap();
        assert!(
            save_many_to(
                &path,
                Snowflake(1),
                &[UserVolume {
                    user_id: Snowflake(257),
                    percent: 67
                }]
            )
            .unwrap()
        );
        let saved = load_from(&path, Snowflake(1));
        assert_eq!(saved.len(), 256);
        assert!(
            saved
                .iter()
                .any(|v| v.user_id == Snowflake(1) && v.percent == 6)
        );
        assert!(!saved.iter().any(|v| v.user_id == Snowflake(2)));
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn concurrent_writers_preserve_both_accounts() {
        let path = test_path();
        let (first, _) = Writer::at(Some(path.clone()), Snowflake(1));
        let (second, _) = Writer::at(Some(path.clone()), Snowflake(3));
        first.set(Snowflake(2), 6);
        second.set(Snowflake(2), 67);
        first.flush();
        second.flush();
        assert_eq!(first.finish(), Status::Saved);
        assert_eq!(second.finish(), Status::Saved);
        assert_eq!(load_from(&path, Snowflake(1))[0].percent, 6);
        assert_eq!(load_from(&path, Snowflake(3))[0].percent, 67);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn parser_keeps_only_bounded_valid_nonsecret_volume_rows() {
        let parsed = parse("1,2,0\n1,3,200\n1,4,201\nwrong\n2,5,100\n");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].1.percent, 0);
        assert_eq!(parsed[1].1.percent, 200);
        assert_eq!(parsed[2].0, Snowflake(2));
    }
    #[test]
    fn encoded_volumes_round_trip_with_account_isolation() {
        let rows = [
            (
                Snowflake(1),
                UserVolume {
                    user_id: Snowflake(2),
                    percent: 0,
                },
            ),
            (
                Snowflake(3),
                UserVolume {
                    user_id: Snowflake(2),
                    percent: 200,
                },
            ),
        ];
        let parsed = parse(&encode(&rows));
        assert_eq!(for_account(parsed.clone(), Snowflake(1)), [rows[0].1]);
        assert_eq!(for_account(parsed, Snowflake(3)), [rows[1].1]);
    }
    #[test]
    fn saved_volume_survives_reload_and_is_account_scoped() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("fastcord-settings-{}-{unique}", std::process::id()))
            .join("voice-settings.csv");
        let account = Snowflake(11);
        let user = Snowflake(22);
        save_to(&path, account, user, 175).unwrap();
        save_to(&path, Snowflake(33), user, 25).unwrap();
        assert_eq!(
            load_from(&path, account),
            [UserVolume {
                user_id: user,
                percent: 175
            }]
        );
        assert_eq!(
            load_from(&path, Snowflake(33)),
            [UserVolume {
                user_id: user,
                percent: 25
            }]
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn account_rows_beyond_the_bound_keep_the_newest() {
        let rows: Vec<_> = (0..300u64)
            .map(|user| {
                (
                    Snowflake(1),
                    UserVolume {
                        user_id: Snowflake(user + 1),
                        percent: 50,
                    },
                )
            })
            .collect();
        let kept = for_account(rows, Snowflake(1));
        assert_eq!(kept.len(), MAX_ACCOUNT_ENTRIES);
        assert_eq!(kept.first().unwrap().user_id, Snowflake(45));
        assert_eq!(kept.last().unwrap().user_id, Snowflake(300));
    }

    #[test]
    fn purging_an_account_leaves_other_accounts_rows() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("fastcord-purge-{}-{unique}", std::process::id()))
            .join("voice-settings.csv");
        save_to(&path, Snowflake(1), Snowflake(2), 10).unwrap();
        save_to(&path, Snowflake(3), Snowflake(2), 20).unwrap();
        purge_from(&path, Snowflake(1)).unwrap();
        assert!(load_from(&path, Snowflake(1)).is_empty());
        assert_eq!(load_from(&path, Snowflake(3)).len(), 1);
        purge_from(&path, Snowflake(3)).unwrap();
        assert!(!path.exists());
        purge_from(&path, Snowflake(3)).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
