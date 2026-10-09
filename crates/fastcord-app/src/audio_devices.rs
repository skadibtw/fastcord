//! Machine-local, non-secret device preferences and the lazily probed settings view.
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use fastcord_audio::{DeviceChoice, DeviceInfo, DeviceKey, Direction, list_devices};
use iced::widget::{button, column, container, pick_list, row, text};
use iced::{Element, Length};
use serde_json::{Value, json};

const MAX_FILE_BYTES: usize = 8192;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    pub choice: DeviceChoice,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preferences {
    pub input: Selection,
    pub output: Selection,
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
        f.write_str(&self.label)?;
        if self.unavailable {
            f.write_str(" — unavailable")?;
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

pub fn view<'a>(
    settings: &'a Settings,
    prefs: &Preferences,
    input_error: Option<fastcord_audio::DeviceErrorKind>,
    output_error: Option<fastcord_audio::DeviceErrorKind>,
) -> Element<'a, crate::Message> {
    let (inputs, input) = options(&settings.inputs, &prefs.input);
    let (outputs, output) = options(&settings.outputs, &prefs.output);
    container(column![
        text("Audio devices").size(24),
        text("Choose input and output independently. Explicit devices never fall back to speakers."),
        text("Microphone input"),
        pick_list(inputs, Some(input), |option| crate::Message::SelectAudioDevice(Direction::Input, option)).width(Length::Fill),
        text(input_error.map_or_else(String::new, |error| format!("Input stopped: {error}. Select a device to retry."))),
        text(settings.input_error.unwrap_or("")),
        text("Playback output"),
        pick_list(outputs, Some(output), |option| crate::Message::SelectAudioDevice(Direction::Output, option)).width(Length::Fill),
        text(output_error.map_or_else(String::new, |error| format!("Output stopped: {error}. Select a device to retry."))),
        text(settings.output_error.unwrap_or("")),
        text(if settings.probing { "Looking for audio devices…" } else { "System default follows the OS only when selected. Refresh lists after plugging a device in." }),
        row![button("Refresh devices").on_press_maybe((!settings.probing).then_some(crate::Message::RefreshAudioDevices)), button("Close").on_press(crate::Message::CloseAudioSettings)].spacing(12),
    ].spacing(12)).padding(24).width(Length::Fixed(650.0)).style(container::rounded_box).into()
}

fn path() -> Option<PathBuf> {
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
    Some(root.join("fastcord").join("audio-devices.json"))
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
        &json!({"input": selection(&prefs.input)?, "output": selection(&prefs.output)?}),
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
    .map_err(|_| "Could not save audio device choices. Check the local settings directory.")
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
