//! The `discord.com` cookie jar (ADR 0005, IDENTITY_AUDIT A8): what Discord's
//! responses set (`__dcfduid`, `__sdcfduid`, `_cfuvid`, `cf_clearance`, ...)
//! is sent back on later requests to `discord.com`, as the official client's
//! browser profile does. One process-wide jar shared by every account, like
//! that profile. Bounded, in memory only, never logged.
//!
//! Only `discord.com` takes part: the CDN client stays cookie-free, and the
//! `discord.gg` WebSocket hosts are another site whose cookies Discord never
//! sets.

use std::sync::Mutex;

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

const MAX_COOKIES: usize = 32;
const MAX_COOKIE_BYTES: usize = 4096;

static JAR: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Whether `url` is a `discord.com` origin the jar belongs to.
fn applies(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host == "discord.com" || host.ends_with(".discord.com"))
}

/// Keeps the cookies a response from `url` sets.
pub(crate) fn store(url: &Url, headers: &HeaderMap) {
    if !applies(url) {
        return;
    }
    let mut jar = JAR.lock().unwrap_or_else(|e| e.into_inner());
    for value in headers.get_all(SET_COOKIE) {
        if let Ok(text) = value.to_str() {
            apply(&mut jar, text);
        }
    }
}

/// The `Cookie` header for a request to `url`, if the jar holds any.
pub(crate) fn header(url: &Url) -> Option<HeaderValue> {
    if !applies(url) {
        return None;
    }
    let jar = JAR.lock().unwrap_or_else(|e| e.into_inner());
    if jar.is_empty() {
        return None;
    }
    let line = jar
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    let mut value = HeaderValue::from_str(&line).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// One `Set-Cookie` line: `name=value` is kept or replaced, and `Max-Age` of
/// zero or less deletes it. Other attributes are not modelled.
// ponytail: no expiry clock (`Expires`, positive `Max-Age`); the jar lives only
// as long as the process, like a session.
fn apply(jar: &mut Vec<(String, String)>, line: &str) {
    if line.len() > MAX_COOKIE_BYTES {
        return;
    }
    let mut parts = line.split(';');
    let Some((name, value)) = parts.next().and_then(|pair| pair.split_once('=')) else {
        return;
    };
    let (name, value) = (name.trim(), value.trim());
    let token = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_graphic() && !matches!(b, b';' | b',' | b'"' | b'\\'))
    };
    if name.is_empty() || !token(name) || !token(value) {
        return;
    }
    let deleted = parts.any(|attribute| {
        attribute.trim().split_once('=').is_some_and(|(key, age)| {
            key.trim().eq_ignore_ascii_case("max-age")
                && age.trim().parse::<i64>().is_ok_and(|age| age <= 0)
        })
    });
    jar.retain(|(held, _)| held != name);
    if !deleted && jar.len() < MAX_COOKIES {
        jar.push((name.to_owned(), value.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_are_kept_replaced_deleted_and_bounded() {
        let mut jar = Vec::new();
        apply(
            &mut jar,
            "__dcfduid=abc123; Expires=Fri, 23 Oct 2027 10:00:00 GMT; Max-Age=31536000; Secure; HttpOnly; Path=/; SameSite=Lax",
        );
        apply(
            &mut jar,
            "_cfuvid=x.y-z; path=/; domain=.discord.com; HttpOnly; Secure; SameSite=None",
        );
        assert_eq!(
            jar,
            [
                ("__dcfduid".to_owned(), "abc123".to_owned()),
                ("_cfuvid".to_owned(), "x.y-z".to_owned())
            ]
        );
        apply(&mut jar, "__dcfduid=new; Path=/");
        assert_eq!(jar[1], ("__dcfduid".to_owned(), "new".to_owned()));
        apply(&mut jar, "_cfuvid=; Max-Age=0");
        assert_eq!(jar.len(), 1);
        // Malformed or hostile lines change nothing.
        for line in ["", "novalue", "=v", "a b=c", "a=b\r\nX: y", "a=\"q\""] {
            apply(&mut jar, line);
        }
        apply(&mut jar, &format!("big={}", "v".repeat(MAX_COOKIE_BYTES)));
        assert_eq!(jar.len(), 1);
        for i in 0..100 {
            apply(&mut jar, &format!("c{i}=v"));
        }
        assert_eq!(jar.len(), MAX_COOKIES);
    }

    #[test]
    fn only_discord_com_over_https_takes_part() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(applies(&url("https://discord.com/api/v9/users/@me")));
        assert!(applies(&url("https://canary.discord.com/app")));
        for other in [
            "http://discord.com/",
            "https://cdn.discordapp.com/x",
            "https://gateway.discord.gg/",
            "https://evildiscord.com/",
            "https://discord.com.evil.example/",
        ] {
            assert!(!applies(&url(other)), "{other}");
        }
    }
}
