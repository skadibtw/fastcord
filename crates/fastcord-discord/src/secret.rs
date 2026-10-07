use std::fmt;

use zeroize::Zeroizing;

/// An owned user credential, redacted in Debug and zeroized on drop.
///
/// HTTP/TLS and native-store internals may retain copies; this is not a guarantee
/// that every copy of a credential can be erased. There is deliberately no
/// Display, serde implementation, or implicit conversion back to a string.
pub struct UserToken(Zeroizing<String>);

impl UserToken {
    pub fn new(token: String) -> Self {
        Self(Zeroizing::new(token))
    }

    /// Expose only to authentication protocols or the native credential store.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for UserToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UserToken([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_debug_is_redacted() {
        let token = UserToken::new("offline-test-credential".to_owned());
        assert_eq!(format!("{token:?}"), "UserToken([REDACTED])");
        assert_eq!(token.expose_secret(), "offline-test-credential");
    }
}
