//! A simple per-route limit: more than [`LIMIT`] hits within [`WINDOW`] get a
//! 429, so the client's back-off can be watched against the fake.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

pub const LIMIT: usize = 5;
pub const WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct RateLimiter {
    hits: HashMap<String, VecDeque<Instant>>,
}

impl RateLimiter {
    /// `None` if the request may proceed (and is counted); otherwise how long
    /// until the oldest hit leaves the window. Refused requests are not counted.
    pub fn check(&mut self, key: &str, now: Instant) -> Option<Duration> {
        let hits = self.hits.entry(key.to_owned()).or_default();
        while hits
            .front()
            .is_some_and(|first| now.duration_since(*first) >= WINDOW)
        {
            hits.pop_front();
        }
        if hits.len() >= LIMIT {
            let wait = WINDOW - now.duration_since(hits[0]);
            return Some(wait.max(Duration::from_millis(50)));
        }
        hits.push_back(now);
        None
    }
}

/// The route a request counts against: method and path, with ids replaced
/// except the major one (the id right after `channels` or `guilds`).
pub fn route_key(method: &str, path: &str) -> String {
    let mut key = String::from(method);
    let mut previous = "";
    for segment in path.split('/').filter(|s| !s.is_empty()) {
        key.push('/');
        let id = !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
        if id && !matches!(previous, "channels" | "guilds") {
            key.push_str("{id}");
        } else {
            key.push_str(segment);
        }
        previous = segment;
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sixth_hit_in_a_second_is_refused_and_the_window_slides() {
        let mut limiter = RateLimiter::default();
        let start = Instant::now();
        for i in 0..LIMIT {
            assert!(
                limiter
                    .check("r", start + Duration::from_millis(i as u64))
                    .is_none()
            );
        }
        let wait = limiter
            .check("r", start + Duration::from_millis(100))
            .unwrap();
        assert!(
            wait <= WINDOW && wait >= Duration::from_millis(800),
            "{wait:?}"
        );
        // Another route is independent.
        assert!(limiter.check("other", start).is_none());
        // After the window the route is free again.
        assert!(
            limiter
                .check("r", start + WINDOW + Duration::from_millis(10))
                .is_none()
        );
    }

    #[test]
    fn keys_keep_the_major_id_and_normalize_the_rest() {
        assert_eq!(
            route_key("GET", "/api/v9/channels/12/messages/34"),
            "GET/api/v9/channels/12/messages/{id}"
        );
        assert_eq!(
            route_key("PUT", "/api/v9/guilds/5/roles/7"),
            "PUT/api/v9/guilds/5/roles/{id}"
        );
        assert_ne!(
            route_key("GET", "/api/v9/channels/1/messages"),
            route_key("GET", "/api/v9/channels/2/messages")
        );
    }
}
