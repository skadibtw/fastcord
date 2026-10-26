//! Typing indicators: `POST /channels/{id}/typing` for the account's own
//! typing, and the small expiring table of who else is typing.
//!
//! Both are pure over a caller-supplied monotonic time (`Clock::now`), so they
//! are tested without waiting. The POST is never retried by the transport.

use std::time::Duration;

use fastcord_model::Snowflake;

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// How long one TYPING_START shows (Discord's own clients use about 10 s).
pub const TYPING_LIFETIME: Duration = Duration::from_secs(10);
/// Least time between two typing POSTs for one channel.
const SEND_INTERVAL: Duration = Duration::from_secs(8);
/// Most people tracked in one channel, and in all channels.
const MAX_PER_CHANNEL: usize = 4;
const MAX_TOTAL: usize = 32;

impl<C: Clock> RestClient<C> {
    /// Tells `channel` the account is typing (it lasts about ten seconds).
    pub async fn trigger_typing(&self, channel: Snowflake) -> Result<(), RestError> {
        let route = Route::new(Method::POST, &format!("/channels/{channel}/typing"))?;
        self.execute(RestRequest::new(route, Priority::UserWrite))
            .await
            .map(|_| ())
    }
}

/// Who is typing where, with the time each entry lapses. Never grows past
/// [`MAX_TOTAL`] entries; the oldest go first.
#[derive(Debug, Default)]
pub struct TypingTable {
    /// (channel, user, lapses at), oldest first.
    entries: Vec<(Snowflake, Snowflake, Duration)>,
}

impl TypingTable {
    pub fn start(&mut self, now: Duration, channel: Snowflake, user: Snowflake) {
        self.stop(channel, user);
        let in_channel = self.entries.iter().filter(|e| e.0 == channel).count();
        if in_channel >= MAX_PER_CHANNEL
            && let Some(at) = self.entries.iter().position(|e| e.0 == channel)
        {
            self.entries.remove(at);
        }
        if self.entries.len() >= MAX_TOTAL {
            self.entries.remove(0);
        }
        self.entries.push((channel, user, now + TYPING_LIFETIME));
    }

    /// The user sent a message (or otherwise stopped). True if they were shown.
    pub fn stop(&mut self, channel: Snowflake, user: Snowflake) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !(e.0 == channel && e.1 == user));
        self.entries.len() != before
    }

    /// The channel is gone. True if anyone was shown there.
    pub fn clear_channel(&mut self, channel: Snowflake) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.0 != channel);
        self.entries.len() != before
    }

    /// A new session: nothing seen before it is still true.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Drops lapsed entries everywhere. True if any of `channel`'s went:
    /// only the shown channel's line needs redrawing.
    pub fn prune(&mut self, now: Duration, channel: Option<Snowflake>) -> bool {
        let mut shown = false;
        self.entries.retain(|e| {
            let keep = e.2 > now;
            shown |= !keep && Some(e.0) == channel;
            keep
        });
        shown
    }

    /// Who is typing in `channel` at `now`, longest-typing first.
    pub fn users(&self, now: Duration, channel: Snowflake) -> Vec<Snowflake> {
        self.entries
            .iter()
            .filter(|e| e.0 == channel && e.2 > now)
            .map(|e| e.1)
            .collect()
    }

    /// When the next entry of `channel` lapses; `None` means no timer is
    /// needed. Other channels' entries lapse lazily: [`Self::users`] skips them.
    pub fn next_expiry(&self, channel: Option<Snowflake>) -> Option<Duration> {
        self.entries
            .iter()
            .filter(|e| Some(e.0) == channel)
            .map(|e| e.2)
            .min()
    }
}

/// Throttle for the account's own typing POSTs.
#[derive(Debug, Default)]
pub struct TypingSend {
    last: Option<(Snowflake, Duration)>,
}

impl TypingSend {
    /// True when a POST for `channel` may go out now; records it if so.
    pub fn permit(&mut self, now: Duration, channel: Snowflake) -> bool {
        if let Some((last, at)) = self.last
            && last == channel
            && now < at + SEND_INTERVAL
        {
            return false;
        }
        self.last = Some((channel, now));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const C: Snowflake = Snowflake(1);
    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn route_is_the_channel_typing_endpoint() {
        let route = Route::new(Method::POST, "/channels/7/typing").unwrap();
        assert_eq!(route.key().normalized(), "/channels/{channel_id}/typing");
    }

    #[test]
    fn entries_lapse_refresh_and_stop() {
        let mut t = TypingTable::default();
        t.start(s(0), C, Snowflake(5));
        assert_eq!(t.users(s(9), C), [Snowflake(5)]);
        assert!(t.users(s(10), C).is_empty(), "lapsed exactly at lifetime");
        // A repeat refreshes rather than duplicating.
        t.start(s(8), C, Snowflake(5));
        assert_eq!(t.users(s(15), C), [Snowflake(5)]);
        assert_eq!(t.next_expiry(Some(C)), Some(s(18)));
        assert!(t.stop(C, Snowflake(5)));
        assert!(!t.stop(C, Snowflake(5)));
        assert_eq!(t.next_expiry(Some(C)), None, "idle table needs no timer");
    }

    #[test]
    fn prune_reports_change_only_when_something_lapsed() {
        let mut t = TypingTable::default();
        t.start(s(0), C, Snowflake(5));
        t.start(s(0), Snowflake(2), Snowflake(6));
        assert_eq!(t.next_expiry(Some(Snowflake(3))), None, "nobody shown");
        assert!(!t.prune(s(5), Some(C)));
        assert!(t.prune(s(10), Some(C)));
        assert!(t.users(s(10), C).is_empty());
        t.start(s(10), Snowflake(2), Snowflake(6));
        assert!(
            !t.prune(s(20), Some(C)),
            "another channel lapsing is not shown"
        );
        assert!(t.users(s(20), Snowflake(2)).is_empty());
    }

    #[test]
    fn bounded_per_channel_and_overall_and_clearable() {
        let mut t = TypingTable::default();
        for u in 1..=6 {
            t.start(s(0), C, Snowflake(u));
        }
        assert_eq!(
            t.users(s(1), C),
            [Snowflake(3), Snowflake(4), Snowflake(5), Snowflake(6)]
        );
        for ch in 100..200 {
            t.start(s(0), Snowflake(ch), Snowflake(1));
        }
        assert!(t.entries.len() <= MAX_TOTAL);
        assert!(t.clear_channel(Snowflake(199)));
        t.clear();
        assert_eq!(t.next_expiry(Some(C)), None);
    }

    #[test]
    fn own_typing_is_sent_once_per_interval_per_channel() {
        let mut send = TypingSend::default();
        assert!(send.permit(s(0), C));
        assert!(!send.permit(s(7), C));
        assert!(send.permit(s(8), C));
        assert!(
            send.permit(s(9), Snowflake(2)),
            "another channel is not throttled"
        );
        assert!(
            send.permit(s(10), C),
            "returning to a channel sends at once"
        );
    }
}
