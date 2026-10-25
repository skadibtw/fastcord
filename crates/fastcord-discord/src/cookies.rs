//! The `discord.com` cookie jar (ADR 0005, IDENTITY_AUDIT A8): what Discord's
//! responses set (`__dcfduid`, `__sdcfduid`, `_cfuvid`, `cf_clearance`, ...)
//! is sent back on later requests to the same host and path, as the official
//! client's browser profile does. One process-wide jar shared by every account,
//! like that profile. Bounded, in memory only, never logged.
//!
//! Only `discord.com` takes part: the CDN client stays cookie-free, and the
//! `discord.gg` WebSocket hosts are another site whose cookies Discord never
//! sets. A cookie is scoped by its `Domain` and `Path` attributes, expires with
//! `Max-Age`/`Expires` on the request clock, and dies with the process when the
//! server sets no expiry (a session cookie). Cookies Discord deliberately cannot
//! scope outside `discord.com` (a foreign `Domain`, an `http` origin) are
//! refused rather than kept.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderValue, SET_COOKIE};

const MAX_COOKIES: usize = 32;
const MAX_COOKIE_BYTES: usize = 4096;
/// Name, value and attributes of every stored cookie together never exceed this.
const MAX_TOTAL_BYTES: usize = 16 * 1024;

static JAR: Mutex<Jar> = Mutex::new(Jar::new());

/// One stored cookie. `expires` is Unix seconds; `None` is a session cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Cookie {
    name: String,
    value: String,
    /// Lower case, without the leading dot of the `Domain` attribute.
    domain: String,
    /// The `Domain` attribute was absent: only the exact host matches.
    host_only: bool,
    path: String,
    expires: Option<u64>,
}

impl Cookie {
    fn bytes(&self) -> usize {
        self.name.len() + self.value.len() + self.domain.len() + self.path.len()
    }

    fn expired(&self, now: u64) -> bool {
        self.expires.is_some_and(|at| now >= at)
    }

    /// RFC 6265 §5.1.3 domain matching, restricted to `discord.com` and its
    /// subdomains by [`applies`].
    fn domain_matches(&self, host: &str) -> bool {
        if self.host_only {
            host == self.domain
        } else {
            host == self.domain || host.ends_with(&format!(".{}", self.domain))
        }
    }

    /// RFC 6265 §5.1.4 path matching.
    fn path_matches(&self, request_path: &str) -> bool {
        request_path == self.path
            || (request_path.starts_with(&self.path)
                && (self.path.ends_with('/')
                    || request_path.as_bytes().get(self.path.len()) == Some(&b'/')))
    }
}

/// The bounded set of stored cookies, oldest first.
struct Jar {
    cookies: Vec<Cookie>,
}

impl Jar {
    const fn new() -> Self {
        Self {
            cookies: Vec::new(),
        }
    }

    fn bytes(&self) -> usize {
        self.cookies.iter().map(Cookie::bytes).sum()
    }

    fn store(&mut self, url: &Url, headers: &HeaderMap, now: u64) {
        for value in headers.get_all(SET_COOKIE) {
            if let Ok(text) = value.to_str() {
                self.apply(text, url, now);
            }
        }
        self.prune(now);
    }

    fn apply(&mut self, line: &str, url: &Url, now: u64) {
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
        let Some(host) = url.host_str() else {
            return;
        };
        let host = host.to_ascii_lowercase();
        let mut domain = host.clone();
        let mut host_only = true;
        let mut path = default_path(url.path());
        let mut expires = None;
        let mut max_age = None;
        for attribute in parts {
            let attribute = attribute.trim();
            let (key, value) = match attribute.split_once('=') {
                Some((key, value)) => (key.trim(), Some(value.trim())),
                None => (attribute, None),
            };
            if key.eq_ignore_ascii_case("domain") {
                let Some(value) = value else { continue };
                let value = value
                    .trim_matches(|c| c == '.' || c == '\t' || c == ' ')
                    .to_ascii_lowercase();
                // A cookie may only be scoped to this host or a parent domain.
                if value.is_empty()
                    || !value.contains('.')
                    || !(host == value || host.ends_with(&format!(".{value}")))
                    || value.parse::<std::net::IpAddr>().is_ok()
                {
                    return;
                }
                domain = value;
                host_only = false;
            } else if key.eq_ignore_ascii_case("path") {
                if let Some(value) = value
                    && value.starts_with('/')
                    && value.len() <= MAX_COOKIE_BYTES
                    && token(value)
                {
                    path = value.to_owned();
                }
            } else if key.eq_ignore_ascii_case("max-age") {
                if let Some(seconds) = value.and_then(|v| v.parse::<i64>().ok()) {
                    max_age = Some(seconds);
                }
            } else if key.eq_ignore_ascii_case("expires") {
                if let Some(seconds) = value.and_then(parse_cookie_date) {
                    expires = Some(seconds);
                }
            }
        }
        // `Max-Age` wins over `Expires` (RFC 6265 §5.3).
        expires = match max_age {
            Some(seconds) if seconds <= 0 => None,
            Some(seconds) => Some(now.saturating_add(seconds as u64)),
            None => expires,
        };
        let deleted = max_age.is_some_and(|seconds| seconds <= 0);
        self.cookies
            .retain(|held| !(held.name == name && held.domain == domain && held.path == path));
        if deleted || (max_age.is_none() && expires.is_some_and(|at| now >= at)) {
            return;
        }
        if self.cookies.len() >= MAX_COOKIES {
            self.cookies.remove(0);
        }
        let cookie = Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            domain,
            host_only,
            path,
            expires,
        };
        if cookie.bytes() > MAX_TOTAL_BYTES {
            return;
        }
        while self.bytes() + cookie.bytes() > MAX_TOTAL_BYTES && !self.cookies.is_empty() {
            self.cookies.remove(0);
        }
        self.cookies.push(cookie);
    }

    fn prune(&mut self, now: u64) {
        self.cookies.retain(|cookie| !cookie.expired(now));
    }

    /// RFC 6265 §5.4: longer paths first, then creation order.
    fn header(&mut self, url: &Url, now: u64) -> Option<String> {
        let host = url.host_str()?.to_ascii_lowercase();
        let path = request_path(url.path());
        self.prune(now);
        let mut matching: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|cookie| cookie.domain_matches(&host) && cookie.path_matches(&path))
            .collect();
        matching.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.len()));
        if matching.is_empty() {
            return None;
        }
        Some(
            matching
                .iter()
                .map(|cookie| format!("{}={}", cookie.name, cookie.value))
                .collect::<Vec<_>>()
                .join("; "),
        )
    }
}

/// Whether `url` is a `discord.com` origin the jar belongs to.
fn applies(url: &Url) -> bool {
    url.scheme() == "https"
        && url
            .host_str()
            .is_some_and(|host| host == "discord.com" || host.ends_with(".discord.com"))
}

/// The RFC 6265 §5.1.4 default path of a `Set-Cookie` without a `Path`.
fn default_path(url_path: &str) -> String {
    if !url_path.starts_with('/') {
        return "/".to_owned();
    }
    match url_path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(end) => url_path[..end].to_owned(),
    }
}

/// The request path, never empty and always absolute.
fn request_path(path: &str) -> String {
    if path.is_empty() {
        "/".to_owned()
    } else if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

/// Keeps the cookies a response from `url` sets.
pub(crate) fn store(url: &Url, headers: &HeaderMap) {
    if !applies(url) {
        return;
    }
    let mut jar = JAR.lock().unwrap_or_else(|e| e.into_inner());
    jar.store(url, headers, now_seconds());
}

/// The `Cookie` header for a request to `url`, if the jar holds any that apply.
pub(crate) fn header(url: &Url) -> Option<HeaderValue> {
    if !applies(url) {
        return None;
    }
    let mut jar = JAR.lock().unwrap_or_else(|e| e.into_inner());
    let line = jar.header(url, now_seconds())?;
    let mut value = HeaderValue::from_str(&line).ok()?;
    value.set_sensitive(true);
    Some(value)
}

fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// An `Expires` value in the forms servers actually send: `Wdy, DD Mon YYYY
/// HH:MM:SS GMT`, with the weekday, the comma and the case optional. An
/// unparsable date is ignored rather than treated as an immediate expiry.
fn parse_cookie_date(text: &str) -> Option<u64> {
    let mut day = None;
    let mut month = None;
    let mut year = None;
    let mut time = None;
    for token in text.split(|c: char| c == ' ' || c == ',' || c == '\t' || c == '-') {
        if token.is_empty() {
            continue;
        }
        if let Some((hours, minutes, seconds)) = parse_cookie_time(token) {
            time = Some((hours, minutes, seconds));
        } else if let Some(value) = parse_cookie_month(token) {
            month = Some(value);
        } else if let Ok(value) = token.parse::<u32>() {
            if (1..=31).contains(&value) {
                day = Some(value);
            } else if (1970..=9999).contains(&value) {
                year = Some(value);
            }
        }
    }
    let (day, month, year, (hours, minutes, seconds)) = (day?, month?, year?, time?);
    let days = days_from_civil(year as i64, month, day as i64)?;
    Some(
        (days * 86_400 + i64::from(hours) * 3_600 + i64::from(minutes) * 60 + i64::from(seconds))
            .max(0) as u64,
    )
}

fn parse_cookie_time(token: &str) -> Option<(u32, u32, u32)> {
    let mut parts = token.split(':');
    let hours = parts.next()?.parse::<u32>().ok()?;
    let minutes = parts.next()?.parse::<u32>().ok()?;
    let seconds = parts.next()?.parse::<u32>().ok()?;
    (parts.next().is_none() && hours < 24 && minutes < 60 && seconds < 61)
        .then_some((hours, minutes, seconds))
}

fn parse_cookie_month(token: &str) -> Option<i64> {
    let names = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let token = token.to_ascii_lowercase();
    names
        .iter()
        .position(|name| *name == token)
        .map(|index| index as i64 + 1)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`). `None` for a date that does not exist.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day > days_in_month {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-25T00:00:00Z: a fixed clock for every expiry test.
    const NOW: u64 = 1_792_886_400;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn headers(lines: &[&str]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for line in lines {
            headers.append(SET_COOKIE, HeaderValue::from_str(line).unwrap());
        }
        headers
    }

    fn cookie(name: &str, value: &str) -> Cookie {
        Cookie {
            name: name.to_owned(),
            value: value.to_owned(),
            domain: "discord.com".to_owned(),
            host_only: true,
            path: "/".to_owned(),
            expires: None,
        }
    }

    #[test]
    fn cookies_are_kept_replaced_deleted_and_bounded() {
        let mut jar = Jar::new();
        let url = url("https://discord.com/app");
        jar.store(
            &url,
            &headers(&[
                "__dcfduid=abc123; Expires=Fri, 23 Oct 2027 10:00:00 GMT; Max-Age=31536000; Secure; HttpOnly; Path=/; SameSite=Lax",
                "_cfuvid=x.y-z; path=/; domain=.discord.com; HttpOnly; Secure; SameSite=None",
            ]),
            NOW,
        );
        assert_eq!(
            jar.cookies,
            [
                Cookie {
                    expires: Some(NOW + 31_536_000),
                    ..cookie("__dcfduid", "abc123")
                },
                Cookie {
                    domain: "discord.com".to_owned(),
                    host_only: false,
                    ..cookie("_cfuvid", "x.y-z")
                }
            ]
        );
        jar.apply("__dcfduid=new; Path=/", &url, NOW);
        assert_eq!(jar.cookies[1], cookie("__dcfduid", "new"));
        jar.apply("_cfuvid=; Max-Age=0", &url, NOW);
        assert_eq!(jar.cookies.len(), 1);
        // Malformed or hostile lines change nothing.
        for line in ["", "novalue", "=v", "a b=c", "a=b\r\nX: y", "a=\"q\""] {
            jar.apply(line, &url, NOW);
        }
        jar.apply(&format!("big={}", "v".repeat(MAX_COOKIE_BYTES)), &url, NOW);
        assert_eq!(jar.cookies.len(), 1);
        for i in 0..100 {
            jar.apply(&format!("c{i}=v"), &url, NOW);
        }
        assert_eq!(jar.cookies.len(), MAX_COOKIES);
        assert!(jar.bytes() <= MAX_TOTAL_BYTES);
    }

    #[test]
    fn only_discord_com_over_https_takes_part() {
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

    #[test]
    fn a_cookie_is_scoped_by_domain_and_path() {
        let mut jar = Jar::new();
        let app = url("https://discord.com/app");
        jar.store(
            &app,
            &headers(&[
                "a=1",
                // A parent domain: sent to discord.com and every subdomain.
                "b=2; Domain=.discord.com",
                // Not ours to scope: refused, never sent anywhere.
                "c=3; Domain=canary.discord.com",
                // Scoped to one path.
                "d=4; Path=/api/v9",
            ]),
            NOW,
        );
        let cookie = |jar: &mut Jar, s: &str| jar.header(&url(s), NOW);
        assert_eq!(
            cookie(&mut jar, "https://discord.com/app").as_deref(),
            Some("a=1; b=2")
        );
        // A host-only cookie is not sent to another subdomain; a parent-domain
        // one is. A response from `discord.com` cannot scope a cookie to
        // `canary.discord.com`, so `c=3` was refused.
        assert_eq!(
            cookie(&mut jar, "https://canary.discord.com/app").as_deref(),
            Some("b=2")
        );
        assert_eq!(cookie(&mut jar, "https://cdn.discord.com/x").as_deref(), Some("b=2"));
        // Path matching is on whole segments, and longer paths come first.
        assert_eq!(
            cookie(&mut jar, "https://discord.com/api/v9/users").as_deref(),
            Some("d=4; a=1; b=2")
        );
        assert_eq!(cookie(&mut jar, "https://discord.com/api/v9x").as_deref(), Some("a=1; b=2"));
        // A foreign Domain is refused outright, not stored for another host.
        jar.apply("e=5; Domain=evil.example", &app, NOW);
        assert!(jar.cookies.iter().all(|cookie| cookie.name != "e"));
        // Nor can a domain-scoped cookie escape discord.com's subdomains.
        jar.apply("f=6; Domain=discord.com", &app, NOW);
        assert!(jar.cookies.iter().any(|cookie| cookie.name == "f"));
    }

    #[test]
    fn expiry_follows_the_clock_and_max_age_wins_over_expires() {
        let mut jar = Jar::new();
        let app = url("https://discord.com/app");
        jar.store(
            &app,
            &headers(&[
                "short=1; Max-Age=60",
                "soon=2; Expires=Sun, 25 Oct 2026 00:00:30 GMT",
                "both=3; Max-Age=86400; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
                "session=4",
            ]),
            NOW,
        );
        assert_eq!(
            jar.header(&app, NOW + 29).as_deref(),
            Some("short=1; soon=2; both=3; session=4")
        );
        // Both the relative and the absolute cookie are gone one minute on;
        // an expired one is dropped from memory, not only from the header.
        assert_eq!(
            jar.header(&app, NOW + 61).as_deref(),
            Some("both=3; session=4")
        );
        assert!(jar.cookies.iter().all(|cookie| cookie.name != "short"));
        assert!(jar.cookies.iter().all(|cookie| cookie.name != "soon"));
        // A deletion replaces an earlier value instead of keeping both.
        jar.apply("both=; Max-Age=0", &app, NOW);
        assert!(jar.cookies.iter().all(|cookie| cookie.name != "both"));
        // A stored absolute expiry is honoured without another Set-Cookie.
        jar.apply("later=5; Expires=Fri, 23 Oct 2027 10:00:00 GMT", &app, NOW);
        let before = NOW + 1_000_000;
        assert!(
            jar.header(&app, before)
                .is_some_and(|header| header.contains("later=5"))
        );
        // 1_824_285_600 is the parsed 2027-10-23T10:00:00Z expiry: from there
        // on it is gone, while the session cookie is still there.
        assert!(
            !jar.header(&app, 1_824_285_600)
                .is_some_and(|header| header.contains("later=5"))
        );
        assert!(
            jar.header(&app, 1_824_285_600)
                .is_some_and(|header| header.contains("session=4"))
        );
    }

    #[test]
    fn cookies_are_replaced_per_name_domain_and_path_not_per_name() {
        let mut jar = Jar::new();
        let app = url("https://discord.com/app");
        jar.store(
            &app,
            &headers(&["k=root; Path=/", "k=api; Path=/api/v9"]),
            NOW,
        );
        assert_eq!(jar.cookies.len(), 2);
        assert_eq!(
            jar.header(&url("https://discord.com/api/v9/users"), NOW).as_deref(),
            Some("k=api; k=root")
        );
        // The same name on one path is replaced; the other path keeps its value.
        jar.apply("k=replaced; Path=/", &app, NOW);
        assert_eq!(
            jar.header(&url("https://discord.com/api/v9/users"), NOW).as_deref(),
            Some("k=api; k=replaced")
        );
        assert_eq!(jar.header(&url("https://discord.com/app"), NOW).as_deref(), Some("k=replaced"));
    }

    #[test]
    fn the_jar_stays_within_its_byte_budget_by_dropping_the_oldest() {
        let mut jar = Jar::new();
        let app = url("https://discord.com/app");
        // 32 cookies of ~800 bytes fit only until the byte budget is reached.
        for i in 0..MAX_COOKIES {
            jar.apply(&format!("c{i}={}", "v".repeat(800)), &app, NOW);
        }
        assert!(jar.bytes() <= MAX_TOTAL_BYTES);
        assert!(jar.cookies.len() < MAX_COOKIES);
        // The most recent cookies are the ones kept.
        assert!(jar.cookies.last().is_some_and(|c| c.name == format!("c{}", MAX_COOKIES - 1)));
        // One cookie larger than the whole budget is refused.
        jar.apply(&format!("huge={}", "v".repeat(MAX_TOTAL_BYTES)), &app, NOW);
        assert!(jar.cookies.iter().all(|c| c.name != "huge"));
    }

    #[test]
    fn default_paths_are_directories_and_the_root_for_an_authority() {
        assert_eq!(default_path("/"), "/");
        assert_eq!(default_path("/app"), "/");
        assert_eq!(default_path("/api/v9/users"), "/api/v9");
        assert_eq!(default_path("/api/v9/"), "/api/v9");
        assert_eq!(default_path(""), "/");
        assert_eq!(request_path(""), "/");
        assert_eq!(request_path("/x"), "/x");
        assert_eq!(request_path("x"), "/x");
    }

    #[test]
    fn expires_dates_parse_the_way_servers_send_them() {
        assert_eq!(
            parse_cookie_date("Fri, 23 Oct 2027 10:00:00 GMT"),
            Some(1_824_285_600)
        );
        assert_eq!(
            parse_cookie_date("Wed, 09 Jun 2021 10:18:14 GMT"),
            Some(1_623_233_894)
        );
        // Weekday, comma, case and the trailing zone are all optional.
        assert_eq!(
            parse_cookie_date("09 jun 2021 10:18:14 GMT"),
            Some(1_623_233_894)
        );
        assert_eq!(
            parse_cookie_date("09-Jun-2021 10:18:14 GMT"),
            Some(1_623_233_894)
        );
        // An epoch date and a leap day.
        assert_eq!(parse_cookie_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_cookie_date("Sat, 29 Feb 2020 00:00:00 GMT"),
            Some(1_582_934_400)
        );
        // Nothing plausible: ignored, so the cookie stays a session cookie.
        for bad in [
            "",
            "Fri, 23 Foo 2027 10:00:00 GMT",
            "Fri, 32 Oct 2027 10:00:00 GMT",
            "Fri, 30 Feb 2027 10:00:00 GMT",
            "Fri, 23 Oct 2027 25:00:00 GMT",
            "Fri, 23 Oct 2027",
        ] {
            assert_eq!(parse_cookie_date(bad), None, "{bad}");
        }
    }
}
