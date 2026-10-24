//! Bounded, non-secret network settings. The app stores this document beside
//! audio-devices.json and supplies the same isolated fake-mode directory.
//!
//! No endpoint, destination IP, account, packet, SSID/BSSID, signed URL, proxy
//! password or exact endpoint confirmation is represented by this schema.

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Role;
use crate::selector::{CATALOG_VERSION, TLS_PROFILE_VERSION};
use crate::tls::Strategy;

pub const SETTINGS_VERSION: u32 = 1;
pub const MAX_SETTINGS_BYTES: usize = 64 * 1024;
pub const MAX_NETWORK_PROFILES: usize = 8;
pub const MAX_ROLE_HINTS: usize = 32;
pub const HINT_EXPIRY_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    #[default]
    Off,
    Auto,
    Proxy,
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Off => "Off",
            Self::Auto => "Automatic TLS recovery",
            Self::Proxy => "User proxy",
        })
    }
}

/// A role-level preference from prior TLS tests. It must be confirmed for the
/// actual endpoint/family in memory before being treated as a candidate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleHint {
    pub role: Role,
    strategy: SavedStrategy,
    pub confirmed_at_unix_secs: u64,
}

impl RoleHint {
    pub fn strategy(&self) -> Strategy {
        self.strategy.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum SavedStrategy {
    Direct,
    TlsRecordSplit,
    WriteSplit,
    Combined,
}

impl From<Strategy> for SavedStrategy {
    fn from(strategy: Strategy) -> Self {
        match strategy {
            Strategy::Direct => Self::Direct,
            Strategy::TlsRecordSplit => Self::TlsRecordSplit,
            Strategy::WriteSplit => Self::WriteSplit,
            Strategy::Combined => Self::Combined,
        }
    }
}

impl From<SavedStrategy> for Strategy {
    fn from(strategy: SavedStrategy) -> Self {
        match strategy {
            SavedStrategy::Direct => Self::Direct,
            SavedStrategy::TlsRecordSplit => Self::TlsRecordSplit,
            SavedStrategy::WriteSplit => Self::WriteSplit,
            SavedStrategy::Combined => Self::Combined,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProfile {
    /// A lowercase 256-bit local salted digest supplied by a supported OS
    /// identity backend. No usable identity means process-only confirmations:
    /// callers must not invent a persistent profile shared by every network.
    pub local_network_digest: String,
    #[serde(deserialize_with = "deserialize_hints")]
    pub hints: Vec<RoleHint>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub tls_profile_version: u32,
    pub catalog_version: u32,
    pub mode: Mode,
    /// Laboratory UDP discovery preludes are independent of TLS recovery.
    /// Merely loading settings or selecting Auto never enables this flag.
    pub experimental_udp: bool,
    #[serde(deserialize_with = "deserialize_profiles")]
    pub profiles: Vec<NetworkProfile>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            tls_profile_version: TLS_PROFILE_VERSION,
            catalog_version: CATALOG_VERSION,
            mode: Mode::Off,
            experimental_udp: false,
            profiles: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingsError {
    TooLarge,
    Invalid,
    UnsupportedVersion,
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "Network settings exceed the size limit.",
            Self::Invalid => "Network settings are invalid.",
            Self::UnsupportedVersion => "Network settings use an unsupported version.",
        })
    }
}

impl std::error::Error for SettingsError {}

impl Settings {
    pub fn decode(bytes: &[u8], now_unix_secs: u64) -> Result<Self, SettingsError> {
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsError::TooLarge);
        }
        let mut settings: Self =
            serde_json::from_slice(bytes).map_err(|_| SettingsError::Invalid)?;
        settings.validate()?;
        settings.expire(now_unix_secs);
        Ok(settings)
    }

    pub fn encode(&self, now_unix_secs: u64) -> Result<Vec<u8>, SettingsError> {
        self.validate()?;
        let mut settings = self.clone();
        settings.expire(now_unix_secs);
        let bytes = serde_json::to_vec(&settings).map_err(|_| SettingsError::Invalid)?;
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsError::TooLarge);
        }
        Ok(bytes)
    }

    fn validate(&self) -> Result<(), SettingsError> {
        if self.version != SETTINGS_VERSION {
            return Err(SettingsError::UnsupportedVersion);
        }
        if self.profiles.len() > MAX_NETWORK_PROFILES
            || self
                .profiles
                .iter()
                .map(|profile| profile.hints.len())
                .sum::<usize>()
                > MAX_ROLE_HINTS
        {
            return Err(SettingsError::TooLarge);
        }
        for (index, profile) in self.profiles.iter().enumerate() {
            if !is_digest(&profile.local_network_digest)
                || self.profiles[..index]
                    .iter()
                    .any(|saved| saved.local_network_digest == profile.local_network_digest)
                || profile.hints.iter().enumerate().any(|(index, hint)| {
                    profile.hints[..index]
                        .iter()
                        .any(|saved| saved.role == hint.role)
                })
            {
                return Err(SettingsError::Invalid);
            }
        }
        Ok(())
    }

    fn expire(&mut self, now_unix_secs: u64) {
        if self.tls_profile_version != TLS_PROFILE_VERSION
            || self.catalog_version != CATALOG_VERSION
        {
            self.profiles.clear();
            self.tls_profile_version = TLS_PROFILE_VERSION;
            self.catalog_version = CATALOG_VERSION;
        }
        for profile in &mut self.profiles {
            profile.hints.retain(|hint| {
                hint.confirmed_at_unix_secs <= now_unix_secs
                    && now_unix_secs - hint.confirmed_at_unix_secs < HINT_EXPIRY_SECS
            });
        }
        self.profiles.retain(|profile| !profile.hints.is_empty());
    }

    /// Inserts one role-level hint only when the caller has a genuine local
    /// salted identity. Bounded oldest-first eviction never increases caps.
    pub fn remember_hint(
        &mut self,
        local_network_digest: &str,
        role: Role,
        strategy: Strategy,
        now_unix_secs: u64,
    ) -> Result<(), SettingsError> {
        if !is_digest(local_network_digest) {
            return Err(SettingsError::Invalid);
        }
        self.validate()?;
        self.expire(now_unix_secs);
        let hint = RoleHint {
            role,
            strategy: strategy.into(),
            confirmed_at_unix_secs: now_unix_secs,
        };
        if let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.local_network_digest == local_network_digest)
            && let Some(saved) = profile.hints.iter_mut().find(|saved| saved.role == role)
        {
            *saved = hint;
            return Ok(());
        }
        if self
            .profiles
            .iter()
            .map(|profile| profile.hints.len())
            .sum::<usize>()
            == MAX_ROLE_HINTS
        {
            let oldest = self
                .profiles
                .iter()
                .enumerate()
                .flat_map(|(profile, saved)| {
                    saved
                        .hints
                        .iter()
                        .enumerate()
                        .map(move |(hint, saved)| (saved.confirmed_at_unix_secs, profile, hint))
                })
                .min();
            if let Some((_, profile, hint)) = oldest {
                self.profiles[profile].hints.remove(hint);
            }
            self.profiles.retain(|profile| !profile.hints.is_empty());
        }
        if let Some(profile) = self
            .profiles
            .iter_mut()
            .find(|profile| profile.local_network_digest == local_network_digest)
        {
            profile.hints.push(hint);
        } else {
            if self.profiles.len() == MAX_NETWORK_PROFILES {
                let oldest = self
                    .profiles
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, profile)| {
                        profile
                            .hints
                            .iter()
                            .map(|hint| hint.confirmed_at_unix_secs)
                            .max()
                            .unwrap_or(0)
                    })
                    .map(|(index, _)| index)
                    .unwrap_or(0);
                self.profiles.remove(oldest);
            }
            self.profiles.push(NetworkProfile {
                local_network_digest: local_network_digest.to_owned(),
                hints: vec![hint],
            });
        }
        Ok(())
    }
}

fn is_digest(identity: &str) -> bool {
    identity.len() == 64
        && identity
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn bounded_sequence<'de, D, T, const LIMIT: usize>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct BoundedVisitor<T, const LIMIT: usize>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const LIMIT: usize> Visitor<'de> for BoundedVisitor<T, LIMIT> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "an array with at most {LIMIT} entries")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(LIMIT));
            while let Some(entry) = sequence.next_element()? {
                if entries.len() == LIMIT {
                    return Err(de::Error::custom("network settings entry limit exceeded"));
                }
                entries.push(entry);
            }
            Ok(entries)
        }
    }
    deserializer.deserialize_seq(BoundedVisitor::<T, LIMIT>(std::marker::PhantomData))
}

fn deserialize_profiles<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<NetworkProfile>, D::Error> {
    bounded_sequence::<D, NetworkProfile, MAX_NETWORK_PROFILES>(deserializer)
}

fn deserialize_hints<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<RoleHint>, D::Error> {
    bounded_sequence::<D, RoleHint, MAX_ROLE_HINTS>(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_do_not_enable_udp() {
        let settings = Settings::default();
        assert_eq!(settings.mode, Mode::Off);
        assert!(!settings.experimental_udp);
        let bytes = settings.encode(1000).unwrap();
        assert_eq!(Settings::decode(&bytes, 1000).unwrap(), settings);
    }

    #[test]
    fn role_hints_expire_at_twenty_four_hours_and_on_tls_catalog_changes() {
        let mut settings = Settings::default();
        settings
            .remember_hint(&"a".repeat(64), Role::Gateway, Strategy::WriteSplit, 1000)
            .unwrap();
        let bytes = settings.encode(1000).unwrap();
        assert_eq!(
            Settings::decode(&bytes, 1000 + HINT_EXPIRY_SECS - 1)
                .unwrap()
                .profiles
                .len(),
            1
        );
        assert!(
            Settings::decode(&bytes, 1000 + HINT_EXPIRY_SECS)
                .unwrap()
                .profiles
                .is_empty()
        );
        assert!(Settings::decode(&bytes, 999).unwrap().profiles.is_empty());
        settings.catalog_version = CATALOG_VERSION + 1;
        assert!(
            Settings::decode(&serde_json::to_vec(&settings).unwrap(), 1000)
                .unwrap()
                .profiles
                .is_empty()
        );
        settings.catalog_version = CATALOG_VERSION;
        settings.tls_profile_version = TLS_PROFILE_VERSION + 1;
        assert!(
            Settings::decode(&serde_json::to_vec(&settings).unwrap(), 1000)
                .unwrap()
                .profiles
                .is_empty()
        );
    }

    #[test]
    fn hints_never_store_hosts_raw_network_identity_accounts_or_secrets() {
        let mut settings = Settings::default();
        for canary in [
            "My secret SSID",
            "1.2.3.4",
            "voice.discord.media",
            "token-canary",
            "ABCDEF",
        ] {
            assert_eq!(
                settings.remember_hint(canary, Role::Voice, Strategy::Combined, 1000),
                Err(SettingsError::Invalid)
            );
        }
        let mut value = serde_json::to_value(settings).unwrap();
        value["token"] = serde_json::json!("canary-never-logged");
        let error = Settings::decode(&serde_json::to_vec(&value).unwrap(), 1000).unwrap_err();
        assert!(!format!("{error:?}: {error}").contains("canary"));
    }

    #[test]
    fn profile_hint_and_file_limits_hold_under_repeated_insertions() {
        let mut settings = Settings::default();
        let roles = [
            Role::Rest,
            Role::Gateway,
            Role::RemoteAuth,
            Role::Voice,
            Role::Stream,
            Role::Login,
            Role::Profile,
            Role::Cdn,
            Role::Upload,
        ];
        for network in 0..20 {
            for (offset, role) in roles.iter().enumerate() {
                settings
                    .remember_hint(
                        &format!("{network:064x}"),
                        *role,
                        Strategy::TlsRecordSplit,
                        1000 + offset as u64,
                    )
                    .unwrap();
                assert!(settings.profiles.len() <= MAX_NETWORK_PROFILES);
                assert!(
                    settings
                        .profiles
                        .iter()
                        .map(|profile| profile.hints.len())
                        .sum::<usize>()
                        <= MAX_ROLE_HINTS
                );
            }
        }
        assert!(settings.encode(2000).unwrap().len() <= MAX_SETTINGS_BYTES);
        assert_eq!(
            Settings::decode(&vec![b' '; MAX_SETTINGS_BYTES + 1], 0),
            Err(SettingsError::TooLarge)
        );
        let mut value = serde_json::to_value(&settings).unwrap();
        value["profiles"] = serde_json::json!(vec![settings.profiles[0].clone(); 9]);
        assert!(Settings::decode(&serde_json::to_vec(&value).unwrap(), 2000).is_err());
    }

    #[test]
    fn duplicate_profiles_and_roles_are_rejected() {
        let mut settings = Settings::default();
        settings
            .remember_hint(&"a".repeat(64), Role::Gateway, Strategy::Direct, 1000)
            .unwrap();
        let mut duplicates = settings.clone();
        duplicates.profiles.push(settings.profiles[0].clone());
        assert_eq!(duplicates.encode(1000), Err(SettingsError::Invalid));
        let hint = settings.profiles[0].hints[0].clone();
        settings.profiles[0].hints.push(hint);
        assert_eq!(settings.encode(1000), Err(SettingsError::Invalid));
    }
}
