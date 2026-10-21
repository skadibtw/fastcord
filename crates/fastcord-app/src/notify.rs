//! Desktop-notification policy (SPEC milestone 42): which new messages raise a
//! toast, at most one per [`COOLDOWN`], with the rest coalesced into a text-free
//! "N new messages" summary on the next allowed toast, and a bounded replay
//! filter so Resume never notifies twice. Pure and clock-injected; the toast
//! itself is `fastcord_platform::notify`. No timer or polling: decisions are
//! made only when a message event arrives.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fastcord_model::{Message, Snowflake};

/// Minimum time between toasts.
pub const COOLDOWN: Duration = Duration::from_secs(5);
/// Message IDs remembered to drop replays.
const SEEN: usize = 128;
/// Characters of message text a toast shows.
pub const PREVIEW_CHARS: usize = 80;

pub struct Candidate {
    pub id: Snowflake,
    pub direct: bool,
    pub mentions_me: bool,
    pub own: bool,
    /// The channel or its guild is muted.
    pub muted: bool,
    /// The author is ignored or blocked, or the conversation is a message
    /// request not yet accepted: nothing of it may surface, not even in a
    /// coalesced summary.
    pub suppressed: bool,
    pub author: String,
    pub text: String,
    pub has_attachment: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Toast {
    pub title: String,
    pub body: String,
}

#[derive(Default)]
pub struct Policy {
    seen: VecDeque<Snowflake>,
    last: Option<Instant>,
    coalesced: usize,
}

impl Policy {
    /// `active` is whether the window has focus; `enabled` the user setting.
    pub fn decide(
        &mut self,
        candidate: &Candidate,
        enabled: bool,
        active: bool,
        now: Instant,
    ) -> Option<Toast> {
        if !enabled
            || active
            || candidate.own
            || candidate.suppressed
            || (candidate.muted && !candidate.mentions_me)
            || !(candidate.direct || candidate.mentions_me)
            || self.seen.contains(&candidate.id)
        {
            return None;
        }
        if self.seen.len() == SEEN {
            self.seen.pop_front();
        }
        self.seen.push_back(candidate.id);
        // ponytail: a coalesced message with no later event is never shown;
        // add a one-shot timer if that proves too quiet.
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < COOLDOWN)
        {
            self.coalesced += 1;
            return None;
        }
        self.last = Some(now);
        if std::mem::take(&mut self.coalesced) > 0 {
            return Some(Toast {
                title: "fastcord".to_owned(),
                body: "New messages".to_owned(),
            });
        }
        let body = if !candidate.text.trim().is_empty() {
            fastcord_platform::notify::clip(candidate.text.trim(), PREVIEW_CHARS)
        } else if candidate.has_attachment {
            "Sent an attachment".to_owned()
        } else {
            "New message".to_owned()
        };
        Some(Toast {
            title: candidate.author.clone(),
            body,
        })
    }
}

static POLICY: Mutex<Option<Policy>> = Mutex::new(None);
static ENABLED: AtomicBool = AtomicBool::new(true);
static ACTIVE: AtomicBool = AtomicBool::new(true);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Relaxed);
}

/// Called by the Gateway worker for each created message; shows a toast on a
/// short-lived thread when the policy allows one.
pub fn on_message(message: &Message, me: Option<Snowflake>, muted: bool, suppressed: bool) {
    let Some(me) = me else { return };
    if ACTIVE.load(Ordering::Relaxed) || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let candidate = Candidate {
        id: message.id,
        direct: message.guild_id.is_none(),
        mentions_me: message.mention_everyone || message.mentions.iter().any(|m| m.id == me),
        own: message.author.id == me,
        muted,
        suppressed,
        author: message
            .author
            .global_name
            .clone()
            .unwrap_or_else(|| message.author.username.clone()),
        text: message.content.clone(),
        has_attachment: !message.attachments.is_empty(),
    };
    let toast = POLICY.lock().ok().and_then(|mut policy| {
        policy
            .get_or_insert_with(Policy::default)
            .decide(&candidate, true, false, Instant::now())
    });
    if let Some(toast) = toast {
        crate::ui_sound::play(crate::ui_sound::Sound::Ping);
        std::thread::spawn(move || {
            let _ = fastcord_platform::notify::show(&toast.title, &toast.body);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dm(id: u64) -> Candidate {
        Candidate {
            id: Snowflake(id),
            direct: true,
            mentions_me: false,
            own: false,
            muted: false,
            suppressed: false,
            author: "ann".to_owned(),
            text: "hi".to_owned(),
            has_attachment: false,
        }
    }

    fn decide(policy: &mut Policy, c: &Candidate, now: Instant) -> Option<Toast> {
        policy.decide(c, true, false, now)
    }

    #[test]
    fn only_dms_and_mentions_while_inactive_and_unmuted() {
        let (mut p, t) = (Policy::default(), Instant::now());
        let mut guild = dm(1);
        guild.direct = false;
        assert!(decide(&mut p, &guild, t).is_none(), "plain guild message");
        guild.mentions_me = true;
        assert!(decide(&mut p, &guild, t).is_some());
        assert!(
            Policy::default().decide(&dm(2), true, true, t).is_none(),
            "active"
        );
        assert!(
            Policy::default().decide(&dm(2), false, false, t).is_none(),
            "disabled"
        );
        let mut muted = dm(3);
        muted.muted = true;
        assert!(decide(&mut Policy::default(), &muted, t).is_none());
        let mut own = dm(4);
        own.own = true;
        assert!(decide(&mut Policy::default(), &own, t).is_none());
    }

    #[test]
    fn a_suppressed_message_notifies_nothing_not_even_a_summary_or_a_mention() {
        let (mut p, t) = (Policy::default(), Instant::now());
        assert!(decide(&mut p, &dm(1), t).is_some());
        // Inside the cool-down a suppressed message must not be counted into
        // the "New messages" summary, and a mention does not override it.
        let mut hidden = dm(2);
        hidden.suppressed = true;
        hidden.mentions_me = true;
        assert!(decide(&mut p, &hidden, t).is_none());
        assert!(decide(&mut p, &hidden, t + COOLDOWN).is_none());
        assert_eq!(p.coalesced, 0);
        assert!(p.seen.iter().all(|id| *id != Snowflake(2)));
        assert_eq!(decide(&mut p, &dm(3), t + COOLDOWN).unwrap().body, "hi");
    }

    #[test]
    fn replay_notifies_once() {
        let (mut p, t) = (Policy::default(), Instant::now());
        assert!(decide(&mut p, &dm(1), t).is_some());
        assert!(decide(&mut p, &dm(1), t + COOLDOWN * 2).is_none());
    }

    #[test]
    fn burst_is_rate_limited_then_summarized_without_text() {
        let (mut p, t) = (Policy::default(), Instant::now());
        assert_eq!(decide(&mut p, &dm(1), t).unwrap().body, "hi");
        assert!(decide(&mut p, &dm(2), t).is_none());
        assert!(decide(&mut p, &dm(3), t).is_none());
        let summary = decide(&mut p, &dm(4), t + COOLDOWN).unwrap();
        assert_eq!(summary.body, "New messages");
        assert_eq!(decide(&mut p, &dm(5), t + COOLDOWN * 2).unwrap().body, "hi");
    }

    #[test]
    fn preview_and_replay_memory_are_bounded() {
        let (mut p, mut t) = (Policy::default(), Instant::now());
        let mut long = dm(1);
        long.text = "x".repeat(10_000);
        let toast = decide(&mut p, &long, t).unwrap();
        assert_eq!(toast.body.chars().count(), PREVIEW_CHARS + 1);
        for n in 2..1000 {
            t += COOLDOWN;
            decide(&mut p, &dm(n), t);
        }
        assert!(p.seen.len() <= SEEN);
        let mut file = dm(2000);
        file.text.clear();
        file.has_attachment = true;
        assert_eq!(
            decide(&mut p, &file, t + COOLDOWN).unwrap().body,
            "Sent an attachment"
        );
    }
}
