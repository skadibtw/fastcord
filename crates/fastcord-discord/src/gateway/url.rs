use std::fmt;

use reqwest::Url;

use super::compression::Compression;

/// Gateway protocol version. Separate from the voice Gateway version.
pub(crate) const GATEWAY_VERSION: &str = "9";

/// A validated Gateway WebSocket URL with the connection parameters fixed:
/// API v9 (the official client's), JSON encoding, zlib-stream transport compression.
///
/// Both `GET /gateway` and READY's `resume_gateway_url` are server-provided;
/// the identify/resume payloads carry the user token, so only `wss://` URLs on
/// `discord.gg` hosts are ever connected to.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct GatewayUrl(Url);

impl GatewayUrl {
    pub(crate) fn parse(base: &str) -> Option<Self> {
        let mut url = Url::parse(base).ok()?;
        let host = url.host_str()?;
        #[cfg(feature = "fake-discord")]
        let fake = crate::fake_origin::is_gateway(&url);
        #[cfg(not(feature = "fake-discord"))]
        let fake = false;
        if !fake
            && (url.scheme() != "wss"
                || !(host == "discord.gg" || host.ends_with(".discord.gg"))
                || !url.username().is_empty()
                || url.password().is_some()
                || url.port().is_some_and(|port| port != 443)
                || url.fragment().is_some())
        {
            return None;
        }
        url.query_pairs_mut()
            .clear()
            .append_pair("v", GATEWAY_VERSION)
            .append_pair("encoding", "json")
            .append_pair("compress", Compression::ZlibStream.query_value());
        Some(Self(url))
    }

    pub(crate) fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for GatewayUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Hosts identify a region, not a secret, but keep URLs out of logs anyway.
        f.write_str("GatewayUrl(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_hosts_get_the_fixed_query() {
        for base in [
            "wss://gateway.discord.gg",
            "wss://gateway.discord.gg/",
            "wss://gateway-us-east1-b.discord.gg/?v=6&encoding=etf",
            "wss://gateway.discord.gg:443",
        ] {
            let url = GatewayUrl::parse(base).unwrap_or_else(|| panic!("{base}"));
            assert!(
                url.as_str()
                    .ends_with("/?v=9&encoding=json&compress=zlib-stream"),
                "{}",
                url.as_str()
            );
            assert!(!url.as_str().contains("etf") && !url.as_str().contains("v=6"));
        }
    }

    #[test]
    fn unrelated_hosts_schemes_and_credentials_are_refused() {
        for base in [
            "ws://gateway.discord.gg",
            "https://gateway.discord.gg",
            "wss://discord.gg.evil.example",
            "wss://evildiscord.gg",
            "wss://gateway.discord.com",
            "wss://user@gateway.discord.gg",
            "wss://user:pass@gateway.discord.gg",
            "wss://gateway.discord.gg:8443",
            "wss://gateway.discord.gg/#frag",
            "wss://",
            "gateway.discord.gg",
            "",
        ] {
            assert!(GatewayUrl::parse(base).is_none(), "{base}");
        }
    }
}
