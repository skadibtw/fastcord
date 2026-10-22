//! Read-aloud and Discord TTS messages (issue #104). Everything is spoken by
//! the local synthesizer of `fastcord_platform::speech`; no text reaches a
//! cloud speech service. This module holds the settings, the plain text that
//! is spoken, the policy for received `tts` messages, and the process-wide
//! speaker, which is started on first use off the calling thread.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Mutex;

use fastcord_model::markup::{self, BlockKind, Kind};
use fastcord_model::{Message, Snowflake, Status};
use fastcord_platform::speech::{self, MAX_RATE, MIN_RATE, Speaker, SpeechError, Utterance};
use serde_json::{Value, json};

/// Message IDs remembered so a replayed event is never spoken twice.
const SEEN: usize = 128;
/// Longest saved voice identifier, in bytes.
const MAX_VOICE_ID: usize = 256;

/// Which received TTS messages are spoken; Discord's own three choices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TtsMode {
    #[default]
    Never,
    SelectedChannel,
    AllChannels,
}

impl TtsMode {
    pub const ALL: [Self; 3] = [Self::Never, Self::SelectedChannel, Self::AllChannels];

    fn persisted(self) -> &'static str {
        match self {
            Self::Never => "never",
            Self::SelectedChannel => "selected",
            Self::AllChannels => "all",
        }
    }

    fn from_persisted(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|mode| mode.persisted() == value)
            .unwrap_or_default()
    }
}

impl fmt::Display for TtsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Never => "Never",
            Self::SelectedChannel => "For the selected channel only",
            Self::AllChannels => "All channels",
        })
    }
}

/// The speech settings, kept in the audio settings file next to the chat preferences.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub mode: TtsMode,
    /// `Voice::id`; the system default when absent.
    pub voice: Option<String>,
    /// Percent, within `MIN_RATE..=MAX_RATE`.
    pub rate: u16,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            mode: TtsMode::Never,
            voice: None,
            rate: 100,
        }
    }
}

impl Prefs {
    pub fn from_json(value: &Value) -> Self {
        Self {
            mode: value
                .get("tts")
                .and_then(Value::as_str)
                .map(TtsMode::from_persisted)
                .unwrap_or_default(),
            voice: value
                .get("voice")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= MAX_VOICE_ID)
                .map(str::to_owned),
            rate: value
                .get("rate")
                .and_then(Value::as_u64)
                .map_or(100, |rate| {
                    rate.clamp(MIN_RATE.into(), MAX_RATE.into()) as u16
                }),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"tts": self.mode.persisted(), "voice": self.voice, "rate": self.rate})
    }
}

/// What the policy needs to know about a received message.
pub struct Incoming {
    pub id: Snowflake,
    pub channel: Snowflake,
    /// The channel the user is looking at.
    pub selected: Option<Snowflake>,
    pub tts: bool,
    pub own: bool,
    /// The channel or its guild is muted.
    pub muted: bool,
    /// The author is ignored or blocked, or the conversation is a message request.
    pub suppressed: bool,
    /// The account's status is Do Not Disturb.
    pub dnd: bool,
}

/// Whether a received message is spoken under `mode`. Replays are the caller's
/// concern (`Seen`).
pub fn should_speak(mode: TtsMode, message: &Incoming) -> bool {
    message.tts
        && !message.own
        && !message.muted
        && !message.suppressed
        && !message.dnd
        && match mode {
            TtsMode::Never => false,
            TtsMode::SelectedChannel => message.selected == Some(message.channel),
            TtsMode::AllChannels => true,
        }
}

/// A bounded replay filter.
#[derive(Default)]
struct Seen(VecDeque<Snowflake>);

impl Seen {
    /// Remembers `id`; false when it was already spoken.
    fn first(&mut self, id: Snowflake) -> bool {
        if self.0.contains(&id) {
            return false;
        }
        if self.0.len() == SEEN {
            self.0.pop_front();
        }
        self.0.push_back(id);
        true
    }
}

/// The text of `message` as it is spoken: formatting stripped, mentions as
/// names, spoilers and code not read out, custom emoji by name.
pub fn plain_text(message: &Message) -> String {
    let parsed = markup::parse(&message.content);
    let mut out = String::new();
    let mut push = |piece: &str| {
        let piece = piece.trim();
        if !piece.is_empty() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(piece);
        }
    };
    for block in &parsed.blocks {
        if matches!(block.kind, BlockKind::Code(_)) {
            push("code block");
            continue;
        }
        let mut line = String::new();
        let mut in_spoiler = false;
        for span in &block.spans {
            let piece: std::borrow::Cow<'_, str> = if span.spoiler.is_some() {
                // A spoiler is hidden on screen, so it is not read out either.
                if std::mem::replace(&mut in_spoiler, true) {
                    continue;
                }
                "spoiler".into()
            } else {
                in_spoiler = false;
                match span.kind {
                    Kind::User(id) => message
                        .mentions
                        .iter()
                        .find(|user| user.id == id)
                        .map_or("someone".into(), |user| {
                            user.display_name().to_owned().into()
                        }),
                    Kind::Role(_) => "a role".into(),
                    Kind::Channel(_) => "a channel".into(),
                    Kind::Everyone => "everyone".into(),
                    Kind::Here => "here".into(),
                    Kind::Timestamp { .. } => "a time".into(),
                    // A bare URL is noise when spoken; masked links read their label.
                    Kind::Link(url) if span.text == url => "link".into(),
                    Kind::Link(_) | Kind::Text => emoji_names(span.text).into(),
                }
            };
            line.push_str(&piece);
        }
        push(&line);
    }
    out
}

/// `<:name:id>` and `<a:name:id>` as `name`.
fn emoji_names(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let token = tail.find('>').map(|end| &tail[1..end]).and_then(|inner| {
            let inner = inner.strip_prefix('a').unwrap_or(inner);
            let (name, id) = inner.strip_prefix(':')?.split_once(':')?;
            let valid = !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit());
            valid.then_some(name)
        });
        match token {
            Some(name) => {
                out.push_str(name);
                rest = &tail[tail.find('>').map_or(1, |end| end + 1)..];
            }
            None => {
                out.push('<');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// "<author> said <text>", or what the message carries instead of text.
pub fn announcement(message: &Message) -> String {
    let author = message.author.display_name();
    let text = plain_text(message);
    if !text.is_empty() {
        format!("{author} said {text}")
    } else if !message.attachments.is_empty() {
        format!("{author} sent an attachment")
    } else {
        format!("{author} sent a message")
    }
}

#[derive(Default)]
struct State {
    speaker: Option<Speaker>,
    prefs: Prefs,
    seen: Seen,
    /// Why the speech engine could not start, for the settings panel.
    error: Option<SpeechError>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut guard = STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(guard.get_or_insert_with(State::default))
}

/// Makes `prefs` the settings in force for received messages.
pub fn apply(prefs: &Prefs) {
    with_state(|state| state.prefs = prefs.clone());
}

/// Why speech is unavailable, if starting it failed.
pub fn error() -> Option<SpeechError> {
    with_state(|state| state.error)
}

/// Whether anything is being spoken or waits to be.
pub fn speaking() -> bool {
    STATE.try_lock().is_ok_and(|guard| {
        guard
            .as_ref()
            .and_then(|s| s.speaker.as_ref())
            .is_some_and(Speaker::is_speaking)
    })
}

/// Silences the speaker and clears its queue; a no-op when nothing was ever spoken.
pub fn stop() {
    with_state(|state| {
        if let Some(speaker) = &state.speaker {
            speaker.stop();
        }
    });
}

/// Queues `text` with the saved voice and rate. The speaker starts on a
/// short-lived thread the first time, so the caller never waits for the engine.
pub fn speak(text: String) {
    let queued = with_state(|state| {
        let utterance = Utterance {
            text: text.clone(),
            voice: state.prefs.voice.clone(),
            rate: state.prefs.rate,
        };
        match &state.speaker {
            Some(speaker) => {
                state.error = speaker
                    .speak(utterance)
                    .err()
                    .filter(|e| *e != SpeechError::Full);
                true
            }
            None => false,
        }
    });
    if queued {
        return;
    }
    std::thread::spawn(move || {
        let started = Speaker::start();
        with_state(|state| {
            match started {
                Ok(speaker) if state.speaker.is_none() => state.speaker = Some(speaker),
                Ok(_) => {}
                Err(error) => {
                    state.error = Some(error);
                    return;
                }
            }
            state.error = None;
            if let Some(speaker) = &state.speaker {
                let _ = speaker.speak(Utterance {
                    text,
                    voice: state.prefs.voice.clone(),
                    rate: state.prefs.rate,
                });
            }
        });
    });
}

/// Called by the Gateway worker for each created message: speaks it when it is
/// a TTS message the settings allow.
pub fn on_message(
    message: &Message,
    me: Option<Snowflake>,
    selected: Option<Snowflake>,
    own_status: Option<Status>,
    muted: bool,
    suppressed: bool,
) {
    let Some(me) = me else { return };
    let incoming = Incoming {
        id: message.id,
        channel: message.channel_id,
        selected,
        tts: message.tts,
        own: message.author.id == me,
        muted,
        suppressed,
        dnd: own_status == Some(Status::Dnd),
    };
    if !incoming.tts {
        return;
    }
    let allowed = with_state(|state| {
        should_speak(state.prefs.mode, &incoming) && state.seen.first(incoming.id)
    });
    if allowed {
        speak(announcement(message));
    }
}

/// Installed voices, loaded on a worker when the settings panel opens.
pub async fn load_voices() -> Result<Vec<speech::Voice>, SpeechError> {
    tokio::task::spawn_blocking(speech::voices)
        .await
        .unwrap_or(Err(SpeechError::Failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(content: &str) -> Message {
        serde_json::from_value(json!({
            "id": "5", "channel_id": "6", "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "content": content,
            "author": {"id": "7", "username": "ann", "global_name": "Ann"},
            "mentions": [{"id": "8", "username": "bob"}],
        }))
        .unwrap()
    }

    #[test]
    fn plain_text_strips_formatting_and_names_mentions() {
        let said = |content: &str| plain_text(&message(content));
        assert_eq!(said("**bold** _it_ ~~gone~~ `code`"), "bold it gone code");
        assert_eq!(said("hi <@8> and <@9>"), "hi bob and someone");
        assert_eq!(said("<@&1> <#2> @everyone"), "a role a channel everyone");
        assert_eq!(said("a ||secret|| b"), "a spoiler b");
        assert_eq!(said("look https://example.com/x"), "look link");
        assert_eq!(said("[the docs](https://example.com)"), "the docs");
        assert_eq!(
            said("<:wave:123> <a:party:456> <:bad:x>"),
            "wave party <:bad:x>"
        );
        assert_eq!(
            said("```rust\nfn main() {}\n```\nafter"),
            "code block after"
        );
        assert_eq!(said("# Title\nbody"), "Title body");
    }

    #[test]
    fn announcements_name_the_author() {
        assert_eq!(announcement(&message("hello <@8>")), "Ann said hello bob");
        assert_eq!(announcement(&message("")), "Ann sent a message");
    }

    fn incoming(tts: bool, channel: u64, selected: Option<u64>) -> Incoming {
        Incoming {
            id: Snowflake(1),
            channel: Snowflake(channel),
            selected: selected.map(Snowflake),
            tts,
            own: false,
            muted: false,
            suppressed: false,
            dnd: false,
        }
    }

    #[test]
    fn modes_decide_which_received_messages_are_spoken() {
        let here = incoming(true, 10, Some(10));
        let elsewhere = incoming(true, 11, Some(10));
        let unselected = incoming(true, 11, None);
        assert!(!should_speak(TtsMode::Never, &here));
        assert!(should_speak(TtsMode::SelectedChannel, &here));
        assert!(!should_speak(TtsMode::SelectedChannel, &elsewhere));
        assert!(!should_speak(TtsMode::SelectedChannel, &unselected));
        assert!(should_speak(TtsMode::AllChannels, &elsewhere));
        // Never a message that is not TTS, the user's own, muted, from an
        // ignored author, or while Do Not Disturb.
        let all = TtsMode::AllChannels;
        assert!(!should_speak(all, &incoming(false, 10, Some(10))));
        for blocked in [
            Incoming {
                own: true,
                ..incoming(true, 10, None)
            },
            Incoming {
                muted: true,
                ..incoming(true, 10, None)
            },
            Incoming {
                suppressed: true,
                ..incoming(true, 10, None)
            },
            Incoming {
                dnd: true,
                ..incoming(true, 10, None)
            },
        ] {
            assert!(!should_speak(all, &blocked));
        }
    }

    #[test]
    fn a_replayed_message_is_not_spoken_twice() {
        let mut seen = Seen::default();
        assert!(seen.first(Snowflake(1)));
        assert!(!seen.first(Snowflake(1)));
        for n in 2..(SEEN as u64 + 3) {
            seen.first(Snowflake(n));
        }
        assert!(seen.0.len() <= SEEN);
    }

    #[test]
    fn settings_round_trip_and_default_to_never() {
        assert_eq!(Prefs::from_json(&json!({})), Prefs::default());
        assert_eq!(Prefs::default().mode, TtsMode::Never);
        let custom = Prefs {
            mode: TtsMode::SelectedChannel,
            voice: Some("voice-id".to_owned()),
            rate: 180,
        };
        assert_eq!(Prefs::from_json(&custom.to_json()), custom);
        let all = Prefs {
            mode: TtsMode::AllChannels,
            ..custom
        };
        assert_eq!(Prefs::from_json(&all.to_json()), all);
        let wild = Prefs::from_json(&json!({"tts": "bogus", "voice": "", "rate": 99999}));
        assert_eq!(
            (wild.mode, wild.voice, wild.rate),
            (TtsMode::Never, None, MAX_RATE)
        );
    }
}
