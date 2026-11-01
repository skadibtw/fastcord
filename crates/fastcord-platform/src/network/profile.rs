//! Linux NetworkManager profile identity, never an inferred ISP identity.
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::Duration,
};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::{Connection, MatchRule, MessageStream, Proxy, message::Type};
const SERVICE: &str = "org.freedesktop.NetworkManager";
const PATH: &str = "/org/freedesktop/NetworkManager";
const ACTIVE: &str = "org.freedesktop.NetworkManager.Connection.Active";
const LIMIT: usize = 16;

fn salt(directory: &Path) -> Option<[u8; 32]> {
    use rand_core::RngCore;
    use std::{
        fs::OpenOptions,
        io::{Read, Write},
        os::unix::fs::OpenOptionsExt,
    };
    std::fs::create_dir_all(directory).ok()?;
    let path = directory.join("network-profile-salt");
    let read = || {
        let file = OpenOptions::new().read(true).open(&path).ok()?;
        let mut bytes = Vec::new();
        file.take(33).read_to_end(&mut bytes).ok()?;
        <[u8; 32]>::try_from(bytes).ok()
    };
    if path.exists() {
        return read();
    }
    let mut bytes = [0; 32];
    rand_core::OsRng.try_fill_bytes(&mut bytes).ok()?;
    // An atomic hard-link publication prevents competing instances from
    // replacing each other's salt. A crash before publication leaves no salt.
    let temp = directory.join(format!(".network-profile-salt-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)
        .ok()?;
    let result = file
        .write_all(&bytes)
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::hard_link(&temp, &path));
    drop(file);
    let _ = std::fs::remove_file(&temp);
    if result.is_ok() { Some(bytes) } else { read() }
}

fn digest(salt: &[u8; 32], uuid: &str) -> Option<String> {
    if uuid.len() != 36
        || !uuid.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(b"fastcord-networkmanager-profile-v1\0");
    hash.update(salt);
    hash.update(uuid.to_ascii_lowercase().as_bytes());
    Some(format!("{:x}", hash.finalize()))
}

struct Profile {
    uuid: String,
    active: bool,
    default4: bool,
    default6: bool,
    vpn: bool,
}
fn choose(profiles: &[Profile], salt: &[u8; 32]) -> Option<String> {
    if profiles.len() > LIMIT || profiles.iter().any(|p| p.active && p.vpn) {
        return None;
    }
    let mut chosen: Option<&str> = None;
    for p in profiles
        .iter()
        .filter(|p| p.active && (p.default4 || p.default6))
    {
        if chosen.is_some_and(|uuid| !uuid.eq_ignore_ascii_case(&p.uuid)) {
            return None;
        }
        chosen = Some(&p.uuid);
    }
    digest(salt, chosen?)
}
async fn identity(connection: &Connection, salt: &[u8; 32]) -> Option<String> {
    let manager = Proxy::new(connection, SERVICE, PATH, SERVICE).await.ok()?;
    let paths: Vec<OwnedObjectPath> = manager.get_property("ActiveConnections").await.ok()?;
    if paths.len() > LIMIT {
        return None;
    }
    let mut profiles = Vec::with_capacity(paths.len());
    for path in paths {
        let proxy = Proxy::new(connection, SERVICE, path, ACTIVE).await.ok()?;
        profiles.push(Profile {
            uuid: proxy.get_property("Uuid").await.ok()?,
            active: proxy.get_property::<u32>("State").await.ok()? == 2,
            default4: proxy.get_property("Default").await.ok()?,
            default6: proxy.get_property("Default6").await.ok()?,
            vpn: proxy.get_property("Vpn").await.ok()?,
        });
    }
    choose(&profiles, salt)
}
fn relevant(message: &zbus::Message) -> bool {
    let Ok((interface, changed, invalidated)) =
        message
            .body()
            .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
    else {
        return false;
    };
    if interface != SERVICE && interface != ACTIVE {
        return false;
    }
    [
        "ActiveConnections",
        "PrimaryConnection",
        "State",
        "Uuid",
        "Default",
        "Default6",
        "Vpn",
    ]
    .iter()
    .any(|key| changed.contains_key(*key) || invalidated.iter().any(|k| k == key))
}

/// One event-driven subscription; only salted digests leave this module.
/// Errors/service loss remove identity. No polling, probes or network changes.
pub async fn watch(directory: PathBuf, output: tokio::sync::mpsc::Sender<Option<String>>) {
    let _ = output.send(None).await;
    let Some(salt) = tokio::task::spawn_blocking(move || salt(&directory))
        .await
        .ok()
        .flatten()
    else {
        return;
    };
    let Ok(Ok(connection)) =
        tokio::time::timeout(Duration::from_secs(2), Connection::system()).await
    else {
        return;
    };
    let Ok(rule) = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(SERVICE)
        .and_then(|r| r.interface("org.freedesktop.DBus.Properties"))
        .and_then(|r| r.member("PropertiesChanged"))
    else {
        return;
    };
    let Ok(Ok(mut changes)) = tokio::time::timeout(
        Duration::from_secs(2),
        MessageStream::for_match_rule(rule.build(), &connection, Some(16)),
    )
    .await
    else {
        return;
    };
    let Ok(Ok(bus)) = tokio::time::timeout(
        Duration::from_secs(2),
        zbus::fdo::DBusProxy::new(&connection),
    )
    .await
    else {
        return;
    };
    let Ok(Ok(mut owners)) = tokio::time::timeout(
        Duration::from_secs(2),
        bus.receive_name_owner_changed_with_args(&[(0, SERVICE)]),
    )
    .await
    else {
        return;
    };
    loop {
        let value = tokio::time::timeout(Duration::from_secs(2), identity(&connection, &salt))
            .await
            .ok()
            .flatten();
        if output.send(value).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                message = changes.next() => match message { Some(Ok(m)) if !relevant(&m) => continue, Some(Ok(_)) => break, _ => { let _=output.send(None).await; return; } },
                owner = owners.next() => { if owner.is_none() { let _=output.send(None).await; return; } break; }
            }
        }
        if output.send(None).await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn profile(uuid: &str) -> Profile {
        Profile {
            uuid: uuid.into(),
            active: true,
            default4: true,
            default6: true,
            vpn: false,
        }
    }
    const UUID: &str = "01234567-89ab-cdef-0123-456789abcdef";
    #[test]
    fn profile_digest_is_salted_and_ambiguous_routes_have_no_identity() {
        let one = choose(&[profile(UUID)], &[1; 32]).unwrap();
        assert_eq!(one.len(), 64);
        assert!(!one.contains(UUID));
        assert_ne!(one, choose(&[profile(UUID)], &[2; 32]).unwrap());
        assert!(
            choose(
                &[
                    profile(UUID),
                    profile("11234567-89ab-cdef-0123-456789abcdef")
                ],
                &[1; 32]
            )
            .is_none()
        );
        let mut vpn = profile(UUID);
        vpn.vpn = true;
        assert!(choose(&[vpn], &[1; 32]).is_none());
        assert!(choose(&[], &[1; 32]).is_none());
        assert!(digest(&[1; 32], "not-a-profile").is_none());
    }
    #[tokio::test]
    #[ignore = "read-only native NetworkManager availability check"]
    async fn live_networkmanager_profile_returns_only_a_digest() {
        let connection = Connection::system().await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), identity(&connection, &[7; 32]))
            .await
            .unwrap();
        let digest = result.expect("an unambiguous NetworkManager default profile is required");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn installation_salt_is_stable_and_corrupt_salt_is_not_replaced() {
        let dir =
            std::env::temp_dir().join(format!("fastcord-salt-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let one = salt(&dir).unwrap();
        assert_eq!(salt(&dir), Some(one));
        std::fs::write(dir.join("network-profile-salt"), [1, 2, 3]).unwrap();
        assert_eq!(salt(&dir), None);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
