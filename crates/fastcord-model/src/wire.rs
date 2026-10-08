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

/// Longest nonce Discord accepts and therefore the longest this client keeps.
pub(crate) const MAX_NONCE_CHARS: usize = 25;

/// A message nonce as other clients and bots send it: a string or an integer
/// of up to [`MAX_NONCE_CHARS`] characters. Anything else (a longer string, a
/// float, a boolean, a structure) is dropped as "no nonce" rather than failing
/// the whole message, since the nonce is only ever used to recognize our own.
pub(crate) fn lenient_nonce<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    struct V;
    impl<'de> Visitor<'de> for V {
        type Value = Option<String>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a message nonce")
        }
        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok((v.chars().count() <= MAX_NONCE_CHARS).then(|| v.to_owned()))
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_some<D2: Deserializer<'de>>(self, d: D2) -> Result<Self::Value, D2::Error> {
            d.deserialize_any(V)
        }
        fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
            while seq.next_element::<de::IgnoredAny>()?.is_some() {}
            Ok(None)
        }
        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map
                .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
                .is_some()
            {}
            Ok(None)
        }
    }
    d.deserialize_any(V)
}
