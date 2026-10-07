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
}
