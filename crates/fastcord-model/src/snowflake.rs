use std::fmt;
use std::str::FromStr;

/// Milliseconds between the Unix epoch and the Discord epoch (2015-01-01T00:00:00Z).
pub const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// A Discord snowflake ID. Stored as `u64`; serialized by Discord as a decimal string.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Snowflake(pub u64);

impl Snowflake {
    /// Creation time encoded in the ID, as milliseconds since the Unix epoch.
    pub const fn timestamp_ms(self) -> u64 {
        (self.0 >> 22) + DISCORD_EPOCH_MS
    }
}

impl fmt::Display for Snowflake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseSnowflakeError;

impl fmt::Display for ParseSnowflakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("snowflake must be a non-negative decimal integer")
    }
}

impl std::error::Error for ParseSnowflakeError {}

impl FromStr for Snowflake {
    type Err = ParseSnowflakeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ParseSnowflakeError);
        }
        s.parse().map(Snowflake).map_err(|_| ParseSnowflakeError)
    }
}

impl serde::Serialize for Snowflake {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        crate::wire::serialize_u64_str(self.0, s)
    }
}

impl<'de> serde::Deserialize<'de> for Snowflake {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        crate::wire::deserialize_u64_str(d).map(Snowflake)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_timestamp_from_documented_example() {
        // Example from Discord's API reference.
        let id: Snowflake = "175928847299117063".parse().unwrap();
        assert_eq!(id.timestamp_ms(), 1_462_015_105_796);
    }

    #[test]
    fn rejects_non_decimal_and_overflow() {
        for bad in ["", "-1", "+1", "12a", " 1", "18446744073709551616"] {
            assert_eq!(
                bad.parse::<Snowflake>(),
                Err(ParseSnowflakeError),
                "{bad:?}"
            );
        }
        assert_eq!(
            "18446744073709551615".parse::<Snowflake>(),
            Ok(Snowflake(u64::MAX))
        );
    }

    #[test]
    fn serde_uses_decimal_strings_and_accepts_integers() {
        let id: Snowflake = serde_json::from_str("\"41771983423143937\"").unwrap();
        assert_eq!(id, Snowflake(41_771_983_423_143_937));
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"41771983423143937\"");
        assert_eq!(
            serde_json::from_str::<Snowflake>("7").unwrap(),
            Snowflake(7)
        );
        assert!(serde_json::from_str::<Snowflake>("\"-7\"").is_err());
    }
}
