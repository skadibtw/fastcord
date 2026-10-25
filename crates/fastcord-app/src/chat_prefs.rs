//! Local chat and accessibility preferences: text scale, compact or cozy rows,
//! reduced motion, spoiler/autoplay/embed/media preferences, 12h or 24h time and
//! a high-contrast palette. They live in the audio settings file (`audio_devices`)
//! and are mirrored into one process-wide copy so the view code needs no extra
//! plumbing. The pure methods below are what the views and animation timers ask.

use std::sync::RwLock;

use iced::widget::{button, checkbox, column, pick_list, row, slider, text};
use iced::{Element, Length};
use serde_json::{Value, json};

use crate::Message;
use crate::i18n::tr;

pub const MIN_SCALE: u16 = 85;
pub const MAX_SCALE: u16 = 150;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TimeFormat {
    /// No locale API is read, so this is the 24-hour clock the client always showed.
    #[default]
    System,
    Hour12,
    Hour24,
}

impl TimeFormat {
    const ALL: [Self; 3] = [Self::System, Self::Hour12, Self::Hour24];

    fn persisted(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Hour12 => "12h",
            Self::Hour24 => "24h",
        }
    }

    fn from_persisted(value: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|format| format.persisted() == value)
            .unwrap_or_default()
    }

    pub fn hour12(self) -> bool {
        self == Self::Hour12
    }
}

impl std::fmt::Display for TimeFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::System => tr("System (24-hour)"),
            Self::Hour12 => tr("12-hour"),
            Self::Hour24 => tr("24-hour"),
        })
    }
}

/// How a message row is laid out; see `Prefs::row_layout`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    /// Width of the avatar gutter, `None` when there is no gutter at all.
    pub gutter: Option<f32>,
    pub top: f32,
    pub gap: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    /// Percent of the normal size, always within `MIN_SCALE..=MAX_SCALE`.
    pub text_scale: u16,
    pub compact: bool,
    pub reduced_motion: bool,
    /// Spoilers show their text without a click.
    pub reveal_spoilers: bool,
    /// Animated emoji play (still off under reduced motion).
    pub autoplay: bool,
    pub link_embeds: bool,
    pub inline_media: bool,
    pub time_format: TimeFormat,
    pub high_contrast: bool,
    /// Interface language; `None` follows the operating system.
    pub language: Option<crate::i18n::Lang>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            text_scale: 100,
            compact: false,
            reduced_motion: false,
            reveal_spoilers: false,
            autoplay: true,
            link_embeds: true,
            inline_media: true,
            time_format: TimeFormat::System,
            high_contrast: false,
            language: None,
        }
    }
}

static CURRENT: RwLock<Prefs> = RwLock::new(Prefs {
    text_scale: 100,
    compact: false,
    reduced_motion: false,
    reveal_spoilers: false,
    autoplay: true,
    link_embeds: true,
    inline_media: true,
    time_format: TimeFormat::System,
    high_contrast: false,
    language: None,
});

/// The preferences in force now.
pub fn current() -> Prefs {
    CURRENT
        .read()
        .map_or_else(|poisoned| *poisoned.into_inner(), |p| *p)
}

/// Makes `prefs` the preferences in force (also the clock of `localtime`).
pub fn apply(prefs: Prefs) {
    match CURRENT.write() {
        Ok(mut current) => *current = prefs,
        Err(poisoned) => *poisoned.into_inner() = prefs,
    }
    crate::localtime::set_hour12(prefs.time_format.hour12());
    crate::i18n::set(prefs.language.unwrap_or_else(crate::i18n::Lang::system));
}

impl Prefs {
    pub fn from_json(value: &Value) -> Self {
        let flag = |key, default: bool| value.get(key).and_then(Value::as_bool).unwrap_or(default);
        let default = Self::default();
        Self {
            text_scale: value
                .get("text_scale")
                .and_then(Value::as_u64)
                .map_or(100, |pct| {
                    pct.clamp(MIN_SCALE.into(), MAX_SCALE.into()) as u16
                }),
            compact: flag("compact", default.compact),
            reduced_motion: flag("reduced_motion", default.reduced_motion),
            reveal_spoilers: flag("reveal_spoilers", default.reveal_spoilers),
            autoplay: flag("autoplay", default.autoplay),
            link_embeds: flag("link_embeds", default.link_embeds),
            inline_media: flag("inline_media", default.inline_media),
            time_format: value
                .get("time_format")
                .and_then(Value::as_str)
                .map(TimeFormat::from_persisted)
                .unwrap_or_default(),
            high_contrast: flag("high_contrast", default.high_contrast),
            language: value
                .get("language")
                .and_then(Value::as_str)
                .and_then(crate::i18n::Lang::from_code),
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "text_scale": self.text_scale,
            "compact": self.compact,
            "reduced_motion": self.reduced_motion,
            "reveal_spoilers": self.reveal_spoilers,
            "autoplay": self.autoplay,
            "link_embeds": self.link_embeds,
            "inline_media": self.inline_media,
            "time_format": self.time_format.persisted(),
            "high_contrast": self.high_contrast,
            "language": self.language.map_or("system", crate::i18n::Lang::code),
        })
    }

    pub fn with_scale(mut self, percent: u16) -> Self {
        self.text_scale = percent.clamp(MIN_SCALE, MAX_SCALE);
        self
    }

    /// The iced scale factor: the whole interface, text included, grows together.
    pub fn scale_factor(&self) -> f32 {
        f32::from(self.text_scale.clamp(MIN_SCALE, MAX_SCALE)) / 100.0
    }

    /// Whether emoji frames advance. Reduced motion wins over autoplay.
    pub fn animated(&self) -> bool {
        self.autoplay && !self.reduced_motion
    }

    pub fn spoiler_hidden(&self, revealed_by_click: bool) -> bool {
        !self.reveal_spoilers && !revealed_by_click
    }

    /// Compact rows have no avatar gutter (the header line already carries the
    /// time inline) and tighter spacing; cozy rows keep the 40 px gutter.
    pub fn row_layout(&self, continues: bool) -> RowLayout {
        if self.compact {
            RowLayout {
                gutter: None,
                top: if continues { 0.0 } else { 4.0 },
                gap: 8.0,
            }
        } else {
            RowLayout {
                gutter: Some(40.0),
                top: if continues { 1.0 } else { 14.0 },
                gap: 14.0,
            }
        }
    }
}

/// `h:mm` for a 24-hour hour, in either clock.
pub fn clock(hour: u8, minute: u8, hour12: bool) -> String {
    if hour12 {
        let h = match hour % 12 {
            0 => 12,
            h => h,
        };
        format!("{h}:{minute:02} {}", if hour < 12 { "AM" } else { "PM" })
    } else {
        format!("{hour:02}:{minute:02}")
    }
}

/// A voice in the settings list; `id` is `None` for the system default.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VoiceChoice {
    id: Option<String>,
    label: String,
}

impl std::fmt::Display for VoiceChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label)
    }
}

/// What the text-to-speech rows of the panel show.
pub struct SpeechView<'a> {
    pub prefs: &'a crate::speech::Prefs,
    /// Installed voices once listed.
    pub voices: &'a [fastcord_platform::speech::Voice],
    /// Why speech is unavailable, if it is.
    pub error: Option<fastcord_platform::speech::SpeechError>,
    pub speaking: bool,
}

fn speech_rows<'a>(speech: &SpeechView<'a>) -> Element<'a, Message> {
    let default = VoiceChoice {
        id: None,
        label: tr("System default").to_owned(),
    };
    let mut choices = vec![default.clone()];
    choices.extend(speech.voices.iter().map(|voice| VoiceChoice {
        id: Some(voice.id.clone()),
        label: format!("{} ({})", voice.name, voice.language),
    }));
    // A saved voice that is no longer installed stays visible, not silently replaced.
    let selected = match &speech.prefs.voice {
        None => default,
        Some(id) => choices
            .iter()
            .find(|choice| choice.id.as_ref() == Some(id))
            .cloned()
            .unwrap_or_else(|| {
                let missing = VoiceChoice {
                    id: Some(id.clone()),
                    label: tr("Saved voice (not installed)").to_owned(),
                };
                choices.push(missing.clone());
                missing
            }),
    };
    let prefs = speech.prefs.clone();
    let for_voice = prefs.clone();
    let for_rate = prefs.clone();
    let note = match speech.error {
        Some(error) => tr(&error.to_string()).to_owned(),
        None => tr("Speech is produced on this computer; no text is sent anywhere. Start a message with /tts to send it as a Discord text-to-speech message.").to_owned(),
    };
    column![
        text(tr("Text-to-speech")).size(13).font(crate::ui::BOLD).color(crate::style::SUBTLE),
        row![
            text(tr("Read received text-to-speech messages aloud")),
            pick_list(crate::speech::TtsMode::ALL, Some(prefs.mode), move |mode| {
                Message::SpeechPrefs(crate::speech::Prefs {
                    mode,
                    ..prefs.clone()
                })
            }),
        ]
        .spacing(12),
        row![
            text(tr("Voice")),
            pick_list(choices, Some(selected), move |choice: VoiceChoice| {
                Message::SpeechPrefs(crate::speech::Prefs {
                    voice: choice.id,
                    ..for_voice.clone()
                })
            }),
        ]
        .spacing(12),
        row![
            text(tr("Speaking rate")),
            slider(
                fastcord_platform::speech::MIN_RATE..=fastcord_platform::speech::MAX_RATE,
                speech.prefs.rate,
                move |rate| Message::SpeechPrefs(crate::speech::Prefs {
                    rate,
                    ..for_rate.clone()
                })
            )
            .step(10u16)
            .width(Length::Fill),
            text(format!("{}%", speech.prefs.rate)),
        ]
        .spacing(12),
        row![
            button(text(tr("Test voice"))).on_press(Message::SpeechTest).style(crate::style::secondary_button(false)),
            button(text(tr("Stop"))).on_press_maybe(speech.speaking.then_some(Message::StopSpeech)).style(crate::style::secondary_button(false)),
        ]
        .spacing(12),
        text(note).size(12).color(crate::style::MUTED),
    ]
    .spacing(12)
    .into()
}

/// The language choice: the system's or a fixed one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LanguageChoice(Option<crate::i18n::Lang>);

impl std::fmt::Display for LanguageChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            None => write!(
                f,
                "{} ({})",
                tr("Same as the system"),
                crate::i18n::Lang::system()
            ),
            Some(lang) => write!(f, "{lang}"),
        }
    }
}

/// The "Chat and appearance" page of the settings.
pub fn view<'a>(prefs: Prefs, speech: &SpeechView<'a>) -> Element<'a, Message> {
    let set = move |edit: fn(&mut Prefs, bool)| {
        move |on| {
            let mut next = prefs;
            edit(&mut next, on);
            Message::ChatPrefs(next)
        }
    };
    let languages: Vec<LanguageChoice> = std::iter::once(LanguageChoice(None))
        .chain(crate::i18n::Lang::ALL.map(|lang| LanguageChoice(Some(lang))))
        .collect();
    column![
        crate::audio_devices::section(
            tr("Language"),
            [pick_list(languages, Some(LanguageChoice(prefs.language)), move |choice| {
                let mut next = prefs;
                next.language = choice.0;
                Message::ChatPrefs(next)
            })
            .width(Length::Fill)
            .into()],
        ),
        crate::audio_devices::section(
            tr("Appearance"),
            [
                row![
                    text(tr("Text and interface size")),
                    slider(MIN_SCALE..=MAX_SCALE, prefs.text_scale, move |v| {
                        Message::ChatPrefs(prefs.with_scale(v))
                    })
                    .step(5u16)
                    .width(Length::Fill),
                    text(format!("{}%", prefs.text_scale)),
                ]
                .spacing(12)
                .into(),
                checkbox(prefs.compact)
                    .label(tr("Compact messages (no avatars, tighter spacing)"))
                    .on_toggle(set(|p, on| p.compact = on))
                    .into(),
                checkbox(prefs.high_contrast)
                    .label(tr("High-contrast colors"))
                    .on_toggle(set(|p, on| p.high_contrast = on))
                    .into(),
                row![
                    text(tr("Time format")),
                    pick_list(TimeFormat::ALL, Some(prefs.time_format), move |format| {
                        let mut next = prefs;
                        next.time_format = format;
                        Message::ChatPrefs(next)
                    }),
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center)
                .into(),
            ],
        ),
        crate::audio_devices::section(
            tr("Messages"),
            [
                checkbox(prefs.link_embeds)
                    .label(tr("Show link embeds and previews"))
                    .on_toggle(set(|p, on| p.link_embeds = on))
                    .into(),
                checkbox(prefs.inline_media)
                    .label(tr("Show images, audio and video in the conversation"))
                    .on_toggle(set(|p, on| p.inline_media = on))
                    .into(),
                checkbox(prefs.reveal_spoilers)
                    .label(tr("Always reveal spoilers"))
                    .on_toggle(set(|p, on| p.reveal_spoilers = on))
                    .into(),
                checkbox(prefs.autoplay)
                    .label(tr("Play animated emoji"))
                    .on_toggle(set(|p, on| p.autoplay = on))
                    .into(),
                checkbox(prefs.reduced_motion)
                    .label(tr("Reduced motion (animated emoji show their first frame)"))
                    .on_toggle(set(|p, on| p.reduced_motion = on))
                    .into(),
            ],
        ),
        speech_rows(speech),
        text(tr(
            "Tab moves between controls and Space toggles them. Screen readers and spellcheck are not supported yet."
        ))
        .size(12)
        .color(crate::style::MUTED),
    ]
    .spacing(28)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_round_trip() {
        assert_eq!(Prefs::from_json(&json!({})), Prefs::default());
        let custom = Prefs {
            text_scale: 130,
            compact: true,
            reduced_motion: true,
            reveal_spoilers: true,
            autoplay: false,
            link_embeds: false,
            inline_media: false,
            time_format: TimeFormat::Hour12,
            high_contrast: true,
            language: Some(crate::i18n::Lang::Russian),
        };
        assert_eq!(Prefs::from_json(&custom.to_json()), custom);
        assert_eq!(
            Prefs::from_json(&json!({"time_format": "bogus", "compact": "yes"})),
            Prefs::default()
        );
    }

    #[test]
    fn text_scale_is_clamped_everywhere() {
        assert_eq!(Prefs::from_json(&json!({"text_scale": 10})).text_scale, 85);
        assert_eq!(
            Prefs::from_json(&json!({"text_scale": 9999})).text_scale,
            150
        );
        assert_eq!(Prefs::default().with_scale(0).text_scale, 85);
        assert_eq!(Prefs::default().with_scale(500).scale_factor(), 1.5);
        assert_eq!(Prefs::default().scale_factor(), 1.0);
    }

    #[test]
    fn compact_layout_drops_the_gutter_and_tightens() {
        let cozy = Prefs::default();
        let compact = Prefs {
            compact: true,
            ..cozy
        };
        assert_eq!(cozy.row_layout(false).gutter, Some(40.0));
        assert_eq!(compact.row_layout(false).gutter, None);
        assert!(compact.row_layout(false).top < cozy.row_layout(false).top);
        assert!(compact.row_layout(true).top < cozy.row_layout(true).top);
    }

    #[test]
    fn content_preferences_gate_what_is_shown() {
        let p = Prefs::default();
        assert!(p.spoiler_hidden(false) && !p.spoiler_hidden(true));
        assert!(
            !Prefs {
                reveal_spoilers: true,
                ..p
            }
            .spoiler_hidden(false)
        );
        assert!(p.animated());
        assert!(
            !Prefs {
                autoplay: false,
                ..p
            }
            .animated()
        );
        // Reduced motion stops animation even with autoplay on.
        assert!(
            !Prefs {
                reduced_motion: true,
                ..p
            }
            .animated()
        );
    }

    #[test]
    fn clocks() {
        assert_eq!(clock(16, 5, false), "16:05");
        assert_eq!(clock(16, 5, true), "4:05 PM");
        assert_eq!(clock(0, 30, true), "12:30 AM");
        assert_eq!(clock(12, 0, true), "12:00 PM");
        assert!(TimeFormat::Hour12.hour12() && !TimeFormat::System.hour12());
    }
}
