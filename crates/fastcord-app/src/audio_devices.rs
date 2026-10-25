//! Machine-local, non-secret device preferences and the lazily probed settings view.
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use fastcord_audio::{
    DeviceChoice, DeviceInfo, DeviceKey, Direction, EngineStats, NoiseSuppression,
    NoiseSuppressionStatus, list_devices,
};
use iced::widget::{checkbox, column, pick_list, row, slider, text};

use crate::i18n::{tr, trf};
use iced::{Element, Length};
use serde_json::{Value, json};

const MAX_FILE_BYTES: usize = 8192;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub choice: DeviceChoice,
    pub label: String,
}

/// The saved camera choices: its stable ID (`None` is the first camera) and the
/// label it had, for display when it is not connected (issue #84), plus the
/// background effect and whether the self-view is mirrored (issue #85). None of
/// them turns the camera on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraSelection {
    pub id: Option<String>,
    pub label: String,
    pub effect: fastcord_video::effect::Effect,
    pub mirror: bool,
}

impl Default for CameraSelection {
    fn default() -> Self {
        Self {
            id: None,
            label: String::new(),
            effect: fastcord_video::effect::Effect::Off,
            mirror: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preferences {
    pub input: Selection,
    pub output: Selection,
    pub noise_suppression: NoiseSuppression,
    pub echo_cancellation: bool,
    /// Manual and automatic microphone gain.
    pub input_gain: fastcord_audio::InputGain,
    /// Desktop notifications are on unless this is set (so `Default` is on).
    pub notifications_off: bool,
    /// Toasts name the author but never show message text.
    pub notification_previews_off: bool,
    /// Experimental voice-message recording is offered (off until the user turns it on).
    pub voice_recording: bool,
    /// Input mode, sensitivity and communication shortcuts.
    pub voice_input: crate::shortcuts::Settings,
    /// The chosen camera. The camera itself is never saved as on.
    pub camera: CameraSelection,
    /// Received soundboard volume, mute and favourite sounds.
    pub soundboard: crate::soundboard::Prefs,
    /// Short tones for mute, deafen, joining and leaving, and pings (on by default).
    pub interface_sounds: bool,
    /// Their volume, 0 to 100.
    pub interface_volume: u8,
    /// Chat display and accessibility preferences.
    pub chat: crate::chat_prefs::Prefs,
    /// Open folders, folded categories and hidden muted channels, per account.
    pub organize: crate::organize::Local,
    /// Text-to-speech mode, voice and rate.
    pub speech: crate::speech::Prefs,
    /// Closing the window hides it to the tray icon instead of quitting (off by default).
    pub close_to_tray: bool,
    /// Discord through the embedded byedpi, applied at the next launch.
    pub bypass: crate::desktop::Bypass,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            input: Selection::default(),
            output: Selection::default(),
            noise_suppression: NoiseSuppression::default(),
            echo_cancellation: true,
            input_gain: fastcord_audio::InputGain::default(),
            notifications_off: false,
            notification_previews_off: false,
            voice_recording: false,
            voice_input: crate::shortcuts::Settings::default(),
            camera: CameraSelection::default(),
            soundboard: crate::soundboard::Prefs::default(),
            interface_sounds: true,
            interface_volume: crate::ui_sound::DEFAULT_VOLUME,
            chat: crate::chat_prefs::Prefs::default(),
            organize: crate::organize::Local::default(),
            speech: crate::speech::Prefs::default(),
            close_to_tray: false,
            bypass: crate::desktop::Bypass::default(),
        }
    }
}

impl Preferences {
    pub fn select(&mut self, direction: Direction, option: DeviceOption) {
        let selection = Selection {
            choice: option.choice,
            label: option.label,
        };
        match direction {
            Direction::Input => self.input = selection,
            Direction::Output => self.output = selection,
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceOption {
    pub choice: DeviceChoice,
    pub label: String,
    pub unavailable: bool,
}
impl PartialEq for DeviceOption {
    fn eq(&self, other: &Self) -> bool {
        self.choice == other.choice
    }
}
impl Eq for DeviceOption {}
impl fmt::Display for DeviceOption {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(tr(&self.label))?;
        if self.unavailable {
            f.write_str(tr(" — unavailable"))?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub input: Result<Vec<DeviceInfo>, &'static str>,
    pub output: Result<Vec<DeviceInfo>, &'static str>,
}

#[derive(Default)]
pub struct Settings {
    pub open: bool,
    pub probing: bool,
    pub inputs: Vec<DeviceInfo>,
    pub outputs: Vec<DeviceInfo>,
    pub input_error: Option<&'static str>,
    pub output_error: Option<&'static str>,
}

pub async fn enumerate() -> Inventory {
    tokio::task::spawn_blocking(|| Inventory {
        input: list_devices(Direction::Input)
            .map_err(|_| "Could not enumerate input devices. Refresh to retry."),
        output: list_devices(Direction::Output)
            .map_err(|_| "Could not enumerate output devices. Refresh to retry."),
    })
    .await
    .unwrap_or(Inventory {
        input: Err("Audio device enumeration worker failed."),
        output: Err("Audio device enumeration worker failed."),
    })
}

fn options(devices: &[DeviceInfo], saved: &Selection) -> (Vec<DeviceOption>, DeviceOption) {
    let mut rows = vec![DeviceOption {
        choice: DeviceChoice::SystemDefault,
        label: "System default".to_owned(),
        unavailable: false,
    }];
    rows.extend(devices.iter().map(|device| DeviceOption {
        choice: DeviceChoice::Device(device.key.clone()),
        label: device.label.clone(),
        unavailable: false,
    }));
    let selected = rows
        .iter()
        .find(|row| row.choice == saved.choice)
        .cloned()
        .unwrap_or_else(|| {
            let missing = DeviceOption {
                choice: saved.choice.clone(),
                label: saved.label.clone(),
                unavailable: true,
            };
            rows.push(missing.clone());
            missing
        });
    (rows, selected)
}

/// What the dialog says about noise suppression: the running engine's state while a call
/// has an engine, otherwise the saved preference and when it applies.
fn noise_summary(preference: NoiseSuppression, status: Option<NoiseSuppressionStatus>) -> String {
    match status {
        None => trf(
            "Selected: {}. Applied when you join a call.",
            &[&Noise(preference)],
        ),
        Some(status) if status.loading => trf(
            "Loading High quality… {} stays active until it is ready.",
            &[&Noise(status.active)],
        ),
        Some(status) => trf(
            "Active during this call: {} — {} ms algorithmic delay.",
            &[
                &Noise(status.active),
                &format!("{:.0}", status.latency_samples as f32 / 48.0),
            ],
        ),
    }
}

/// A noise suppression mode named in the interface language.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Noise(NoiseSuppression);

impl fmt::Display for Noise {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(tr(&self.0.to_string()))
    }
}

/// Playback health counters of the running call, so audible glitches can be told apart.
fn stats_summary(stats: Option<EngineStats>) -> String {
    match stats {
        None => String::new(),
        Some(s) => trf(
            "This call: {} playback underruns ({} samples of silence), {} lost or late frames concealed, {} device xruns, {} microphone samples dropped. Real-time engine threads: {}, {} callback promotions refused, {} late wake-ups.",
            &[
                &s.output_underruns,
                &s.output_underrun_samples,
                &s.plc_frames,
                &s.device_xruns,
                &s.input_overrun_samples,
                &s.realtime_threads,
                &s.callback_realtime_denied,
                &s.late_ticks,
            ],
        ),
    }
}

/// The newest voice events, so a dropout can be explained from the dialog.
fn voice_log_summary() -> String {
    let lines = crate::voice_log::recent(8);
    if lines.is_empty() {
        return String::new();
    }
    match crate::voice_log::file() {
        Some(path) => trf(
            "Recent voice events (full log: {}):\n{}",
            &[&path.display(), &lines.join("\n")],
        ),
        None => trf("Recent voice events:\n{}", &[&lines.join("\n")]),
    }
}

/// What the microphone sends in this call, so "nobody hears me" can be told
/// apart: muted or push-to-talk, the sensitivity gate, or end-to-end keys.
pub fn transmit_summary(
    stats: Option<EngineStats>,
    transport: Option<fastcord_media::gateway::TransportStats>,
    capture_on: bool,
) -> String {
    let (Some(s), Some(t)) = (stats, transport) else {
        return String::new();
    };
    let state = if !capture_on {
        tr("Sending is off (muted, deafened, or push-to-talk not held).")
    } else if t.audio_waiting_for_keys > 0 && t.rtp_sent == 0 {
        tr("Sending is waiting for end-to-end encryption keys from the call.")
    } else if s.frames_encoded > 0 && s.frames_gated >= s.frames_encoded {
        tr("Everything is below the input sensitivity threshold.")
    } else {
        tr("Sending.")
    };
    trf(
        "Microphone this call: {} frames encoded, {} held by the sensitivity gate, {} packets sent, {} waiting for encryption keys. {}",
        &[
            &s.frames_encoded,
            &s.frames_gated,
            &t.rtp_sent,
            &t.audio_waiting_for_keys,
            &state,
        ],
    )
}

/// A titled group of a settings page.
pub fn section<'a>(
    title: &'a str,
    rows: impl IntoIterator<Item = Element<'a, crate::Message>>,
) -> Element<'a, crate::Message> {
    let mut body = column![text(title).size(13).font(crate::ui::BOLD).color(crate::style::SUBTLE)]
        .spacing(10);
    for row in rows {
        body = body.push(row);
    }
    body.into()
}

fn hint<'a>(line: impl text::IntoFragment<'a>) -> Element<'a, crate::Message> {
    text(line).size(12).color(crate::style::MUTED).into()
}

/// The input and output devices.
pub fn devices<'a>(
    settings: &'a Settings,
    prefs: &Preferences,
    input_error: Option<fastcord_audio::DeviceErrorKind>,
    output_error: Option<fastcord_audio::DeviceErrorKind>,
) -> Element<'a, crate::Message> {
    let (inputs, input) = options(&settings.inputs, &prefs.input);
    let (outputs, output) = options(&settings.outputs, &prefs.output);
    let mut rows: Vec<Element<'a, crate::Message>> = vec![
        crate::ui::label(tr("INPUT DEVICE")),
        pick_list(inputs, Some(input), |option| {
            crate::Message::SelectAudioDevice(Direction::Input, option)
        })
        .width(Length::Fill)
        .into(),
    ];
    if let Some(error) = input_error {
        rows.push(crate::ui::error(trf(
            "Microphone stopped ({}): {} Choose a device to try again.",
            &[&tr(&error.to_string()), &tr(error.advice())],
        )));
    }
    if let Some(error) = settings.input_error {
        rows.push(crate::ui::error(tr(error)));
    }
    rows.push(crate::ui::label(tr("OUTPUT DEVICE")));
    rows.push(
        pick_list(outputs, Some(output), |option| {
            crate::Message::SelectAudioDevice(Direction::Output, option)
        })
        .width(Length::Fill)
        .into(),
    );
    if let Some(error) = output_error {
        rows.push(crate::ui::error(trf(
            "Playback stopped ({}): {} Choose a device to try again.",
            &[&tr(&error.to_string()), &tr(error.advice())],
        )));
    }
    if let Some(error) = settings.output_error {
        rows.push(crate::ui::error(tr(error)));
    }
    rows.push(
        row![
            hint(if settings.probing {
                tr("Looking for audio devices…")
            } else {
                tr("A chosen device never falls back to another one. Refresh the lists after plugging a device in.")
            }),
            iced::widget::space::horizontal(),
            crate::ui::neutral(
                tr("Refresh"),
                (!settings.probing).then_some(crate::Message::RefreshAudioDevices),
            ),
        ]
        .spacing(12)
        .align_y(iced::Alignment::Center)
        .into(),
    );
    section(tr("Devices"), rows)
}

/// Noise suppression and echo cancellation.
pub fn processing<'a>(
    prefs: &Preferences,
    noise_status: Option<NoiseSuppressionStatus>,
) -> Element<'a, crate::Message> {
    let mut rows: Vec<Element<'a, crate::Message>> = vec![
        crate::ui::label(tr("NOISE SUPPRESSION")),
        pick_list(
            NoiseSuppression::ALL.map(Noise),
            Some(Noise(prefs.noise_suppression)),
            |noise| crate::Message::NoiseSuppressionChanged(noise.0),
        )
        .width(Length::Fill)
        .into(),
        hint(noise_summary(prefs.noise_suppression, noise_status)),
    ];
    if let Some(error) = noise_status.and_then(|status| status.error) {
        rows.push(crate::ui::error(tr(&error.to_string()).to_owned()));
    }
    rows.push(hint(tr(
        "Light uses little CPU. High quality removes keyboard and background voices better but adds about 30 ms of delay and uses more CPU. Applies to the microphone only.",
    )));
    rows.push(
        checkbox(prefs.echo_cancellation)
            .label(tr("Echo cancellation"))
            .on_toggle(crate::Message::EchoCancellationChanged)
            .into(),
    );
    rows.push(hint(tr(
        "Removes what your speakers play from your microphone. Not needed with a headset.",
    )));
    section(tr("Voice processing"), rows)
}

/// Desktop notifications and interface sounds.
pub fn notifications<'a>(prefs: &Preferences) -> Element<'a, crate::Message> {
    section(
        tr("Notifications"),
        [
            checkbox(!prefs.notifications_off)
                .label(tr("Desktop notifications for direct messages and mentions while the window is not active"))
                .on_toggle(crate::Message::NotificationsToggled)
                .into(),
            checkbox(!prefs.notification_previews_off)
                .label(tr("Show message text in notifications"))
                .on_toggle(crate::Message::NotificationPreviewsToggled)
                .into(),
            checkbox(prefs.interface_sounds)
                .label(tr("Interface sounds (mute, deafen, joining and leaving a call, pings)"))
                .on_toggle(crate::Message::InterfaceSoundsToggled)
                .into(),
            row![
                text(tr("Interface sound volume")),
                slider(
                    0..=100u8,
                    prefs.interface_volume,
                    crate::Message::InterfaceVolumeChanged
                )
                .width(Length::Fill),
                text(format!("{}%", prefs.interface_volume)),
            ]
            .spacing(12)
            .into(),
            hint(tr(
                "Notifications for a server or a conversation are set in its menu.",
            )),
        ],
    )
}

/// The experimental voice-message recorder.
pub fn experimental<'a>(prefs: &Preferences) -> Element<'a, crate::Message> {
    section(
        tr("Experimental"),
        [
            checkbox(prefs.voice_recording)
                .label(tr("Voice messages"))
                .on_toggle(crate::Message::VoiceRecordingToggled)
                .into(),
            hint(tr(
                "Adds “Voice message” to the + menu of the message box. Discord documents voice messages for its mobile apps only, so it may refuse them from this client. The microphone is on only while recording.",
            )),
        ],
    )
}

/// Call health counters and the recent voice log, for explaining a problem.
pub fn diagnostics<'a>(
    stats: Option<EngineStats>,
    transmit: String,
) -> Element<'a, crate::Message> {
    let mut rows = Vec::new();
    for line in [stats_summary(stats), transmit, voice_log_summary()] {
        if !line.is_empty() {
            rows.push(text(line).size(12).color(crate::style::MUTED).into());
        }
    }
    if rows.is_empty() {
        rows.push(hint(tr(
            "Call statistics appear here during a call.",
        )));
    }
    section(tr("Call diagnostics"), rows)
}

fn path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join("audio-devices.json"))
}

/// fastcord's per-user data folder (settings, the voice log).
pub(crate) fn data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("APPDATA").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .join("Library/Application Support");
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(root.join(app_dir_name()))
}

/// The per-user directory name under the config and cache roots. The test-only
/// fake mode keeps its own, so it never reads or writes the real app's files.
pub(crate) fn app_dir_name() -> &'static str {
    #[cfg(feature = "fake-discord")]
    if crate::fake::active() {
        return "fastcord-fake";
    }
    "fastcord"
}

fn decode(bytes: &[u8]) -> Option<Preferences> {
    if bytes.len() > MAX_FILE_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(bytes).ok()?;
    fn selection(value: &Value) -> Option<Selection> {
        let label = value.get("label")?.as_str()?;
        if label.len() > 256 {
            return None;
        }
        let choice = match value.get("id")? {
            Value::Null => DeviceChoice::SystemDefault,
            Value::String(id) => DeviceChoice::Device(DeviceKey::from_persisted(id.clone())?),
            _ => return None,
        };
        Some(Selection {
            choice,
            label: label.to_owned(),
        })
    }
    Some(Preferences {
        input: selection(value.get("input")?)?,
        output: selection(value.get("output")?)?,
        // Older device-only settings retain the required Light default.
        noise_suppression: match value.get("noise_suppression") {
            None => NoiseSuppression::Light,
            Some(Value::String(mode)) => NoiseSuppression::from_persisted(mode)?,
            _ => return None,
        },
        // Older settings predate echo cancellation, which is on by default.
        echo_cancellation: match value.get("echo_cancellation") {
            None => true,
            Some(Value::Bool(enabled)) => *enabled,
            _ => return None,
        },
        // Older settings predate gain: 0 dB, automatic gain off.
        input_gain: fastcord_audio::InputGain {
            manual_db: value
                .get("input_gain_db")
                .and_then(Value::as_i64)
                .map_or(0, |db| db.clamp(-128, 127) as i8),
            automatic: value.get("automatic_gain").and_then(Value::as_bool) == Some(true),
        }
        .clamped(),
        notifications_off: value.get("notifications_off").and_then(Value::as_bool) == Some(true),
        notification_previews_off: value
            .get("notification_previews_off")
            .and_then(Value::as_bool)
            == Some(true),
        voice_recording: value.get("voice_recording").and_then(Value::as_bool) == Some(true),
        // Older settings predate voice input: voice activity, no shortcuts.
        voice_input: value
            .get("voice_input")
            .map(crate::shortcuts::Settings::from_json)
            .unwrap_or_default(),
        // Older settings predate cameras: the first camera.
        camera: value
            .get("camera")
            .and_then(camera_selection)
            .unwrap_or_default(),
        // Older settings predate the soundboard: full volume, not muted, no favourites.
        soundboard: value
            .get("soundboard")
            .map(crate::soundboard::Prefs::from_json)
            .unwrap_or_default(),
        // Older settings predate interface sounds: on, at the default volume.
        interface_sounds: value.get("interface_sounds").and_then(Value::as_bool) != Some(false),
        interface_volume: value
            .get("interface_volume")
            .and_then(Value::as_u64)
            .map_or(crate::ui_sound::DEFAULT_VOLUME, |v| v.min(100) as u8),
        // Older settings predate chat preferences: all defaults.
        chat: value
            .get("chat")
            .map(crate::chat_prefs::Prefs::from_json)
            .unwrap_or_default(),
        // Older settings predate the local list layout: nothing folded.
        organize: value
            .get("organize")
            .map(crate::organize::Local::from_json)
            .unwrap_or_default(),
        // Older settings predate text-to-speech: never speak received messages.
        speech: value
            .get("speech")
            .map(crate::speech::Prefs::from_json)
            .unwrap_or_default(),
        // Older settings predate the tray: closing quits.
        close_to_tray: value.get("close_to_tray").and_then(Value::as_bool) == Some(true),
        // Older settings predate the bypass: off.
        bypass: value
            .get("bypass")
            .map(crate::desktop::Bypass::from_json)
            .unwrap_or_default(),
    })
}

fn camera_selection(value: &Value) -> Option<CameraSelection> {
    let label = value.get("label")?.as_str()?;
    let id = match value.get("id")? {
        Value::Null => None,
        // A Windows symbolic link is under 300 bytes.
        Value::String(id) if !id.is_empty() && id.len() <= 512 => Some(id.clone()),
        _ => return None,
    };
    // Older files predate effects and mirroring; an unknown effect name (from
    // a newer build) means no effect.
    let effect = value
        .get("effect")
        .and_then(Value::as_str)
        .and_then(fastcord_video::effect::Effect::from_persisted)
        .unwrap_or_default();
    let mirror = value.get("mirror").and_then(Value::as_bool).unwrap_or(true);
    (label.len() <= 256).then(|| CameraSelection {
        id,
        label: label.to_owned(),
        effect,
        mirror,
    })
}

fn encode(prefs: &Preferences) -> io::Result<Vec<u8>> {
    fn selection(saved: &Selection) -> io::Result<Value> {
        if saved.label.len() > 256 {
            return Err(io::Error::other("audio label exceeds limit"));
        }
        let id = match &saved.choice {
            DeviceChoice::SystemDefault => Value::Null,
            DeviceChoice::Device(key) => Value::String(key.as_str().to_owned()),
        };
        Ok(json!({"id": id, "label": saved.label}))
    }
    let bytes = serde_json::to_vec(
        &json!({"input": selection(&prefs.input)?, "output": selection(&prefs.output)?, "noise_suppression": prefs.noise_suppression.persisted(), "echo_cancellation": prefs.echo_cancellation, "input_gain_db": prefs.input_gain.manual_db, "automatic_gain": prefs.input_gain.automatic, "notifications_off": prefs.notifications_off, "notification_previews_off": prefs.notification_previews_off, "voice_recording": prefs.voice_recording, "voice_input": prefs.voice_input.to_json(), "camera": {"id": prefs.camera.id, "label": prefs.camera.label, "effect": prefs.camera.effect.persisted(), "mirror": prefs.camera.mirror}, "soundboard": prefs.soundboard.to_json(), "interface_sounds": prefs.interface_sounds, "interface_volume": prefs.interface_volume, "chat": prefs.chat.to_json(), "organize": prefs.organize.to_json(), "speech": prefs.speech.to_json(), "close_to_tray": prefs.close_to_tray, "bypass": prefs.bypass.to_json()}),
    )?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(io::Error::other("audio settings exceed limit"));
    }
    Ok(bytes)
}

fn load_from(path: &Path) -> Preferences {
    let read = || -> io::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        fs::File::open(path)?
            .take(MAX_FILE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    read()
        .ok()
        .and_then(|bytes| decode(&bytes))
        .unwrap_or_default()
}
fn save_to(path: &Path, prefs: &Preferences) -> io::Result<()> {
    let bytes = encode(prefs)?;
    fs::create_dir_all(
        path.parent()
            .ok_or_else(|| io::Error::other("settings parent missing"))?,
    )?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(temporary, path)
}

/// The bypass setting, read before the interface starts.
pub fn launch_bypass() -> crate::desktop::Bypass {
    path()
        .map(|path| load_from(&path).bypass)
        .unwrap_or_default()
}

pub async fn load() -> Preferences {
    tokio::task::spawn_blocking(|| {
        path().map_or_else(Preferences::default, |path| load_from(&path))
    })
    .await
    .unwrap_or_default()
}

pub async fn save(prefs: Preferences) -> Result<(), &'static str> {
    tokio::task::spawn_blocking(move || {
        let path = path().ok_or_else(|| io::Error::other("settings path unavailable"))?;
        save_to(&path, &prefs)
    })
    .await
    .map_err(|_| "Audio settings worker failed.")?
    .map_err(|_| "Could not save audio device/noise choices. Check the local settings directory.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_transmit_line_names_why_nothing_is_heard() {
        use fastcord_media::gateway::TransportStats;
        let stats = |encoded, gated| EngineStats {
            frames_encoded: encoded,
            frames_gated: gated,
            ..EngineStats::default()
        };
        let sent = |rtp_sent, waiting| TransportStats {
            rtp_sent,
            audio_waiting_for_keys: waiting,
            ..TransportStats::default()
        };
        assert!(transmit_summary(None, Some(sent(0, 0)), true).is_empty());
        assert!(transmit_summary(Some(stats(10, 0)), None, true).is_empty());
        assert!(
            transmit_summary(Some(stats(50, 0)), Some(sent(0, 0)), false)
                .contains("Sending is off")
        );
        assert!(
            transmit_summary(Some(stats(50, 0)), Some(sent(0, 50)), true)
                .contains("encryption keys from the call")
        );
        assert!(
            transmit_summary(Some(stats(50, 50)), Some(sent(0, 0)), true)
                .contains("below the input sensitivity")
        );
        let ok = transmit_summary(Some(stats(50, 5)), Some(sent(45, 0)), true);
        assert!(ok.contains("45 packets sent") && ok.ends_with("Sending."));
    }
    fn explicit(id: &str, label: &str) -> Selection {
        Selection {
            choice: DeviceChoice::Device(DeviceKey::from_persisted(id).unwrap()),
            label: label.to_owned(),
        }
    }
    #[test]
    fn restart_restores_independent_ids_and_system_default() {
        let path = std::env::temp_dir()
            .join(format!(
                "fastcord-devices-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ))
            .join("audio-devices.json");
        let prefs = Preferences {
            input: explicit("wasapi:microphone", "Duplicate name"),
            output: explicit("wasapi:headset", "Duplicate name"),
            noise_suppression: NoiseSuppression::HighQuality,
            echo_cancellation: false,
            input_gain: fastcord_audio::InputGain {
                manual_db: -7,
                automatic: true,
            },
            notifications_off: true,
            notification_previews_off: true,
            voice_recording: true,
            voice_input: {
                let mut input = crate::shortcuts::Settings::default();
                input.mode = crate::shortcuts::InputMode::PushToTalk;
                input.release_ms = 300;
                input
            },
            camera: CameraSelection {
                id: Some("usb#cam".to_owned()),
                label: "Desk camera".to_owned(),
                ..CameraSelection::default()
            },
            soundboard: crate::soundboard::Prefs {
                volume: 55,
                muted: true,
                favorites: vec![42, 43],
            },
            interface_sounds: false,
            interface_volume: 15,
            chat: crate::chat_prefs::Prefs {
                text_scale: 125,
                compact: true,
                time_format: crate::chat_prefs::TimeFormat::Hour12,
                high_contrast: true,
                ..crate::chat_prefs::Prefs::default()
            },
            organize: {
                let mut local = crate::organize::Local::default();
                local.apply(7, crate::organize::Event::Folder(3));
                local
            },
            speech: crate::speech::Prefs {
                mode: crate::speech::TtsMode::SelectedChannel,
                voice: Some("voice-id".to_owned()),
                rate: 160,
            },
            close_to_tray: true,
            bypass: crate::desktop::Bypass {
                on: true,
                choice: crate::desktop::Choice::Custom,
                options: "-d1 -f-1 -t5".to_owned(),
            },
        };
        save_to(&path, &prefs).unwrap();
        assert_eq!(load_from(&path), prefs);
        let changed = Preferences {
            output: Selection::default(),
            ..prefs
        };
        save_to(&path, &changed).unwrap();
        assert_eq!(load_from(&path), changed);
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
    #[test]
    fn gain_persists_is_clamped_and_older_files_mean_unity() {
        let legacy = br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""}}"#;
        assert_eq!(
            decode(legacy).unwrap().input_gain,
            fastcord_audio::InputGain::default()
        );
        let wild = br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""},"input_gain_db":900,"automatic_gain":true}"#;
        let gain = decode(wild).unwrap().input_gain;
        assert_eq!(gain.manual_db, fastcord_audio::MAX_GAIN_DB);
        assert!(gain.automatic);
    }

    #[test]
    fn missing_explicit_choice_is_displayed_not_replaced_or_matched_by_label() {
        let saved = explicit("headset-a", "Headset");
        let devices = vec![DeviceInfo {
            key: DeviceKey::from_persisted("headset-b").unwrap(),
            label: "Headset".to_owned(),
            is_default: true,
        }];
        let (rows, selected) = options(&devices, &saved);
        assert_eq!(selected.choice, saved.choice);
        assert!(selected.to_string().ends_with("unavailable"));
        assert_eq!(rows.len(), 3);
        let renamed = DeviceOption {
            choice: saved.choice.clone(),
            label: "Renamed".to_owned(),
            unavailable: false,
        };
        assert_eq!(selected, renamed);
    }
    #[test]
    fn older_files_mean_interface_sounds_on_at_the_default_volume() {
        let older =
            decode(br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""}}"#).unwrap();
        assert!(older.interface_sounds);
        assert_eq!(older.interface_volume, crate::ui_sound::DEFAULT_VOLUME);
        let loud = decode(
            br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""},"interface_volume":900}"#,
        )
        .unwrap();
        assert_eq!(loud.interface_volume, 100);
    }
    #[test]
    fn settings_bytes_ids_and_labels_are_bounded() {
        assert!(decode(&vec![b' '; MAX_FILE_BYTES + 1]).is_none());
        assert!(DeviceKey::from_persisted("x".repeat(513)).is_none());
        let prefs = Preferences {
            input: explicit("id", &"x".repeat(257)),
            ..Preferences::default()
        };
        assert!(encode(&prefs).is_err());
        assert!(
            decode(br#"{"input":{"id":"","label":"x"},"output":{"id":null,"label":""}}"#).is_none()
        );
        assert_eq!(
            decode(&encode(&Preferences::default()).unwrap()),
            Some(Preferences::default())
        );
    }
    #[test]
    fn the_camera_choice_persists_is_bounded_and_older_files_mean_the_first_camera() {
        use fastcord_video::effect::{Effect, Preset};
        let prefs = Preferences {
            camera: CameraSelection {
                id: Some("usb#vid_046d&pid_0825#6&1".to_owned()),
                label: "Logitech C270".to_owned(),
                effect: Effect::Preset(Preset::Mint),
                mirror: false,
            },
            ..Preferences::default()
        };
        let restored = decode(&encode(&prefs).unwrap()).unwrap();
        assert_eq!(restored.camera, prefs.camera);
        let legacy = br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""}}"#;
        assert_eq!(decode(legacy).unwrap().camera, CameraSelection::default());
        // A malformed or oversized choice is dropped, not trusted.
        for bad in [
            r#""camera":{"id":"","label":"x"}"#.to_owned(),
            format!(r#""camera":{{"id":"{}","label":"x"}}"#, "a".repeat(513)),
            r#""camera":{"id":3,"label":"x"}"#.to_owned(),
        ] {
            let text = format!(
                r#"{{"input":{{"id":null,"label":""}},"output":{{"id":null,"label":""}},{bad}}}"#
            );
            assert_eq!(
                decode(text.as_bytes()).unwrap().camera,
                CameraSelection::default()
            );
        }
        // Older camera entries predate effects: no effect, mirrored self-view;
        // an effect name from the future is also no effect.
        for entry in [
            r#"{"id":null,"label":""}"#,
            r#"{"id":null,"label":"","effect":"hologram"}"#,
            r#"{"id":null,"label":"","effect":3}"#,
        ] {
            let text = format!(
                r#"{{"input":{{"id":null,"label":""}},"output":{{"id":null,"label":""}},"camera":{entry}}}"#
            );
            let camera = decode(text.as_bytes()).unwrap().camera;
            assert_eq!((camera.effect, camera.mirror), (Effect::Off, true));
        }
        // The camera is a choice of device only: nothing here can turn it on.
        assert!(
            !String::from_utf8(encode(&prefs).unwrap())
                .unwrap()
                .contains("wanted")
        );
    }

    #[test]
    fn noise_modes_persist_and_device_only_settings_default_to_light() {
        for mode in NoiseSuppression::ALL {
            let prefs = Preferences {
                noise_suppression: mode,
                ..Preferences::default()
            };
            assert_eq!(decode(&encode(&prefs).unwrap()), Some(prefs));
        }
        let legacy = br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""}}"#;
        assert_eq!(
            decode(legacy).unwrap().noise_suppression,
            NoiseSuppression::Light
        );
        let invalid = br#"{"input":{"id":null,"label":""},"output":{"id":null,"label":""},"noise_suppression":"krisp"}"#;
        assert!(decode(invalid).is_none());
    }

    #[test]
    fn the_dialog_names_the_running_engine_only_while_a_call_has_one() {
        use NoiseSuppression::{HighQuality, Light, Off};
        // Without an engine the dialog reports the saved choice, never a running mode.
        for preference in [Off, Light, HighQuality] {
            let text = noise_summary(preference, None);
            assert!(text.contains(&preference.to_string()), "{text}");
            assert!(!text.contains("Active during this call"), "{text}");
        }
        let running = NoiseSuppressionStatus {
            requested: HighQuality,
            active: HighQuality,
            loading: false,
            latency_samples: 1_440,
            error: None,
        };
        let text = noise_summary(Off, Some(running));
        assert!(text.contains("Active during this call"), "{text}");
        assert!(text.contains(&HighQuality.to_string()), "{text}");
        assert!(text.contains("30 ms"), "{text}");
        let warming = NoiseSuppressionStatus {
            active: Light,
            loading: true,
            ..running
        };
        let text = noise_summary(HighQuality, Some(warming));
        assert!(text.contains("Loading High quality"), "{text}");
        assert!(text.contains(&Light.to_string()), "{text}");
    }
}
