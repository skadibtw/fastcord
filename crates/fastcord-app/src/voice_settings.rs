use std::fs;
use std::io;
use std::path::PathBuf;

use fastcord_model::Snowflake;

use crate::gateway::UserVolume;

const MAX_FILE_BYTES: u64 = 32 * 1024;
const MAX_ENTRIES: usize = 512;
const MAX_ACCOUNT_ENTRIES: usize = 256;

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
    Some(root.join("fastcord").join("voice-settings.csv"))
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

fn load_from(path: &std::path::Path, account: Snowflake) -> Vec<UserVolume> {
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

pub fn save(account: Snowflake, user: Snowflake, percent: u16) -> io::Result<()> {
    let path = settings_path().ok_or_else(|| io::Error::other("settings path unavailable"))?;
    save_to(&path, account, user, percent)
}

fn save_to(
    path: &std::path::Path,
    account: Snowflake,
    user: Snowflake,
    percent: u16,
) -> io::Result<()> {
    let mut entries = if fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_FILE_BYTES) {
        fs::read_to_string(path).map_or_else(|_| Vec::new(), |text| parse(&text))
    } else {
        Vec::new()
    };
    if let Some((_, volume)) = entries
        .iter_mut()
        .find(|(owner, volume)| *owner == account && volume.user_id == user)
    {
        volume.percent = percent.min(200);
    } else {
        if entries.len() == MAX_ENTRIES {
            entries.remove(0);
        }
        entries.push((
            account,
            UserVolume {
                user_id: user,
                percent: percent.min(200),
            },
        ));
    }
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
    fs::rename(temporary, path)
}

/// Forgets this account's volume rows (logout or removing the saved login).
pub fn purge(account: Snowflake) -> io::Result<()> {
    let path = settings_path().ok_or_else(|| io::Error::other("settings path unavailable"))?;
    purge_from(&path, account)
}

fn purge_from(path: &std::path::Path, account: Snowflake) -> io::Result<()> {
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
