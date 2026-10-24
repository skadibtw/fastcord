//! Desktop-notification policy (SPEC milestone 42): which new messages raise a
//! toast, at most one per [`COOLDOWN`], with the rest coalesced into a text-free
//! "N new messages" summary on the next allowed toast, and a bounded replay
//! filter so Resume never notifies twice. Pure and clock-injected; the toast
//! itself is `fastcord_platform::notify`. No timer or polling: decisions are
//! made only when a message event arrives. Clicking a toast opens its message
//! through the second-launch hand-off queue, as a `discord://` link would.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fastcord_model::{Message, Snowflake};
use fastcord_platform::instance::Forwarded;

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
    /// The account's own status is Do Not Disturb: nothing notifies.
    pub dnd: bool,
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
            || candidate.dnd
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
static PREVIEWS: AtomicBool = AtomicBool::new(true);
/// The hand-off queue `main` also gives the single-instance guard.
static CLICKS: Mutex<Option<tokio::sync::mpsc::Sender<Forwarded>>> = Mutex::new(None);

pub fn set_clicks(sender: tokio::sync::mpsc::Sender<Forwarded>) {
    if let Ok(mut slot) = CLICKS.lock() {
        *slot = Some(sender);
    }
}

/// The link a toast's click opens: the message in its conversation.
fn link(message: &Message) -> String {
    let guild = message
        .guild_id
        .map_or_else(|| "@me".to_owned(), |guild| guild.0.to_string());
    format!(
        "discord://-/channels/{guild}/{}/{}",
        message.channel_id.0, message.id.0
    )
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether toasts may show message text (the author is always named).
pub fn set_previews(previews: bool) {
    PREVIEWS.store(previews, Ordering::Relaxed);
}

pub fn set_active(active: bool) {
    ACTIVE.store(active, Ordering::Relaxed);
}

/// Called by the Gateway worker for each created message; shows a toast on a
/// short-lived thread when the policy allows one.
pub fn on_message(
    message: &Message,
    me: Option<Snowflake>,
    muted: bool,
    suppressed: bool,
    mentions_me: bool,
    dnd: bool,
) {
    let Some(me) = me else { return };
    if ACTIVE.load(Ordering::Relaxed) || !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let previews = PREVIEWS.load(Ordering::Relaxed);
    let candidate = Candidate {
        id: message.id,
        direct: message.guild_id.is_none(),
        mentions_me,
        own: message.author.id == me,
        muted,
        dnd,
        suppressed,
        author: message
            .author
            .global_name
            .clone()
            .unwrap_or_else(|| message.author.username.clone()),
        // Hidden previews: no text, not even whether a file came with it.
        text: if previews {
            message.content.clone()
        } else {
            String::new()
        },
        has_attachment: previews && !message.attachments.is_empty(),
    };
    let toast = POLICY.lock().ok().and_then(|mut policy| {
        policy
            .get_or_insert_with(Policy::default)
            .decide(&candidate, true, false, Instant::now())
    });
    if let Some(toast) = toast {
        crate::ui_sound::play(crate::ui_sound::Sound::Ping);
        let link = link(message);
        let clicks = CLICKS.lock().ok().and_then(|slot| slot.clone());
        std::thread::spawn(move || {
            let _ = fastcord_platform::notify::show(&toast.title, &toast.body, move || {
                if let Some(clicks) = clicks {
                    // A full queue drops the click; the toast is gone either way.
                    let _ = clicks.try_send(Forwarded::Open(link));
                }
            });
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
            dnd: false,
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
        let mut dnd = dm(5);
        dnd.dnd = true;
        dnd.mentions_me = true;
        assert!(decide(&mut Policy::default(), &dnd, t).is_none(), "DND");
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

    #[test]
    fn a_click_opens_the_message_in_its_conversation() {
        let message = |guild: Option<&str>| -> Message {
            serde_json::from_value(serde_json::json!({
                "id": "900", "channel_id": "500", "guild_id": guild, "content": "hi",
                "timestamp": "2026-10-07T12:00:00.000000+00:00",
                "author": {"id": "3", "username": "other"},
            }))
            .unwrap()
        };
        for (guild, expected) in [(Some("7"), Some(Snowflake(7))), (None, None)] {
            assert_eq!(
                crate::desktop::parse(&link(&message(guild))),
                Some(crate::desktop::DeepLink::Channel {
                    guild: expected,
                    channel: Snowflake(500),
                    message: Some(Snowflake(900)),
                })
            );
        }
    }
}
