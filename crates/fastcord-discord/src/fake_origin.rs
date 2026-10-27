//! The test-only loopback origin of the fake Discord server (`fastcord-fake`).
//!
//! Compiled only with the `fake-discord` feature, and never into a release
//! build (`compile_error!` in `lib.rs`). Until [`set_origin`] succeeds, the
//! feature changes nothing. After it, REST goes to `http://127.0.0.1:<port>`
//! and the Gateway to `ws://127.0.0.1:<port>`, and so does the email/password
//! login (`POST /auth/login`, `/auth/mfa/totp`); every other host stays refused,
//! and the remote-auth and CDN paths never connect anywhere.

use std::sync::OnceLock;

use reqwest::Url;

static PORT: OnceLock<u16> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginError {
    /// Not `127.0.0.1:<port>` or `localhost:<port>`.
    NotLoopback,
    /// A different origin was set earlier in this process.
    AlreadySet,
}

/// Sets the one origin for this process: `127.0.0.1:<port>` or `localhost:<port>`.
pub fn set_origin(origin: &str) -> Result<u16, OriginError> {
    let (host, port) = origin.rsplit_once(':').ok_or(OriginError::NotLoopback)?;
    let port: u16 = port.parse().map_err(|_| OriginError::NotLoopback)?;
    if port == 0 || !matches!(host, "127.0.0.1" | "localhost") {
        return Err(OriginError::NotLoopback);
    }
    match PORT.set(port) {
        Ok(()) => Ok(port),
        Err(_) if PORT.get() == Some(&port) => Ok(port),
        Err(_) => Err(OriginError::AlreadySet),
    }
}

pub fn port() -> Option<u16> {
    PORT.get().copied()
}

/// Whether the fake origin is in force in this process.
pub fn active() -> bool {
    PORT.get().is_some()
}

/// `ws://127.0.0.1:<port>/...` for the configured port, without credentials or a fragment.
pub(crate) fn is_gateway(url: &Url) -> bool {
    port().is_some_and(|port| {
        url.scheme() == "ws"
            && url.host_str() == Some("127.0.0.1")
            && url.port() == Some(port)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_loopback_origins_are_accepted() {
        for bad in [
            "discord.com:443",
            "127.0.0.2:80",
            "0.0.0.0:80",
            "example.com:80",
            "127.0.0.1",
            "127.0.0.1:0",
            "127.0.0.1:x",
            "user@127.0.0.1:80",
            "",
        ] {
            assert_eq!(set_origin(bad), Err(OriginError::NotLoopback), "{bad}");
        }
    }
}
