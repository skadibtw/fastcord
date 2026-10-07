//! Serde helpers for Discord's wire conventions.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serializer};

/// Discord sends 64-bit integers (IDs, permission bitsets) as decimal strings.
pub(crate) fn serialize_u64_str<S: Serializer>(v: u64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(&v)
}

/// Accepts a decimal string (normal wire form) or a JSON integer.
pub(crate) fn deserialize_u64_str<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    struct V;
    impl Visitor<'_> for V {
        type Value = u64;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a non-negative integer or decimal string")
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            if v.is_empty() || !v.bytes().all(|b| b.is_ascii_digit()) {
                return Err(E::invalid_value(de::Unexpected::Str(v), &self));
            }
            v.parse()
                .map_err(|_| E::invalid_value(de::Unexpected::Str(v), &self))
        }
    }
    d.deserialize_any(V)
}

/// Distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
/// Use with `#[serde(default, deserialize_with = "double_option")]`.
pub(crate) fn double_option<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}
