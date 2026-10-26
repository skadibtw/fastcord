//! Voice input settings and the communication shortcuts: input mode (voice activity or
//! push-to-talk), sensitivity, the four bindable actions, conflict-aware editing, the
//! binding recorder, and the routing of key edges to call actions. The key model and
//! the OS listener live in `fastcord-platform::shortcut`.
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::i18n::{tr, trf};
use crate::ui::button;
use fastcord_platform::shortcut::{
    Binding, Edge, GlobalShortcuts, Key, Matcher, ShortcutEvent, global_support, mods,
};
use iced::widget::{checkbox, column, pick_list, row, slider, text};
use iced::{Element, Length};
use serde_json::{Value, json};

/// How the microphone is opened to the call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputMode {
    #[default]
    VoiceActivity,
    PushToTalk,
}

impl InputMode {
    pub const ALL: [Self; 2] = [Self::VoiceActivity, Self::PushToTalk];
}

impl fmt::Display for InputMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::VoiceActivity => tr("Voice activity"),
            Self::PushToTalk => tr("Push to talk"),
        })
    }
}

/// Where the bindings are heard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Scope {
    /// Only while the fastcord window has focus; losing focus releases every key.
    #[default]
    Window,
    /// System-wide through the OS listener. Where that is unavailable the shortcuts are
    /// inactive and the reason is shown; they never fall back to the window silently.
    Global,
}

impl Scope {
    pub const ALL: [Self; 2] = [Self::Window, Self::Global];
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Window => tr("Only while this window is focused"),
            Self::Global => tr("System-wide (works while another app is focused)"),
        })
    }
}

/// A bindable communication action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    PushToTalk,
    PushToMute,
    ToggleMute,
    ToggleDeafen,
}

impl Action {
    pub const ALL: [Self; 4] = [
        Self::PushToTalk,
        Self::PushToMute,
        Self::ToggleMute,
        Self::ToggleDeafen,
    ];

    fn index(self) -> usize {
        self as usize
    }

    fn id(self) -> u8 {
        self as u8
    }

    fn from_id(id: u8) -> Option<Self> {
        Self::ALL.get(usize::from(id)).copied()
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::PushToTalk => tr("Push to talk"),
            Self::PushToMute => tr("Push to mute"),
            Self::ToggleMute => tr("Toggle mute"),
            Self::ToggleDeafen => tr("Toggle deafen"),
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::PushToTalk => "push_to_talk",
            Self::PushToMute => "push_to_mute",
            Self::ToggleMute => "toggle_mute",
            Self::ToggleDeafen => "toggle_deafen",
        }
    }
}

/// The persisted voice-input choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub mode: InputMode,
    /// Push-to-talk keeps transmitting this long after release (0-2000 ms).
    pub release_ms: u16,
    /// `None` is automatic: nothing is withheld. `Some(n)` withholds speech quieter
    /// than `-n` dBFS.
    pub sensitivity: Option<u8>,
    pub scope: Scope,
    bindings: [Option<Binding>; 4],
}

pub const MAX_RELEASE_MS: u16 = 2000;
/// Quietest and loudest manual thresholds, as `-n` dBFS.
pub const SENSITIVITY_RANGE: std::ops::RangeInclusive<u8> = 1..=100;
pub const DEFAULT_SENSITIVITY: u8 = 50;

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: InputMode::VoiceActivity,
            release_ms: 200,
            sensitivity: None,
            scope: Scope::Window,
            bindings: [None; 4],
        }
    }
}

impl Settings {
    pub fn binding(&self, action: Action) -> Option<Binding> {
        self.bindings[action.index()]
    }

    /// Binds `action`. A binding another action already uses is refused and that
    /// action returned, so one key can never fire two actions.
    pub fn assign(&mut self, action: Action, binding: Binding) -> Result<(), Action> {
        if let Some(other) = Action::ALL
            .into_iter()
            .find(|other| *other != action && self.binding(*other) == Some(binding))
        {
            return Err(other);
        }
        self.bindings[action.index()] = Some(binding);
        Ok(())
    }

    pub fn clear(&mut self, action: Action) {
        self.bindings[action.index()] = None;
    }

    /// The bindings as the listener takes them.
    pub fn list(&self) -> Vec<(u8, Binding)> {
        Action::ALL
            .into_iter()
            .filter_map(|action| Some((action.id(), self.binding(action)?)))
            .collect()
    }

    pub fn to_json(&self) -> Value {
        let mut bindings = serde_json::Map::new();
        for action in Action::ALL {
            if let Some(binding) = self.binding(action) {
                bindings.insert(action.key().to_owned(), Value::String(binding.to_code()));
            }
        }
        json!({
            "mode": match self.mode { InputMode::VoiceActivity => "voice_activity", InputMode::PushToTalk => "push_to_talk" },
            "release_ms": self.release_ms,
            "sensitivity": self.sensitivity,
            "scope": match self.scope { Scope::Window => "window", Scope::Global => "global" },
            "bindings": bindings,
        })
    }

    /// Reads what [`to_json`](Self::to_json) wrote. Anything unreadable falls back to
    /// its default, and a duplicate binding is dropped, so a hand-edited file cannot
    /// make one key fire two actions.
    pub fn from_json(value: &Value) -> Self {
        let mut settings = Self::default();
        if value.get("mode").and_then(Value::as_str) == Some("push_to_talk") {
            settings.mode = InputMode::PushToTalk;
        }
        if let Some(ms) = value.get("release_ms").and_then(Value::as_u64) {
            settings.release_ms = ms.min(u64::from(MAX_RELEASE_MS)) as u16;
        }
        settings.sensitivity = value
            .get("sensitivity")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 100) as u8);
        if value.get("scope").and_then(Value::as_str) == Some("global") {
            settings.scope = Scope::Global;
        }
        for action in Action::ALL {
            if let Some(binding) = value
                .get("bindings")
                .and_then(|b| b.get(action.key()))
                .and_then(Value::as_str)
                .and_then(Binding::parse)
            {
                let _ = settings.assign(action, binding);
            }
        }
        settings
    }

    /// The warning shown when the chosen mode cannot work.
    pub fn problem(&self) -> Option<&'static str> {
        (self.mode == InputMode::PushToTalk && self.binding(Action::PushToTalk).is_none())
            .then_some(tr(
                "Push to talk is selected but has no key, so you will not transmit.",
            ))
    }
}

/// An edit made in the settings dialog.
#[derive(Clone, Debug)]
pub enum Event {
    Mode(InputMode),
    ReleaseMs(u16),
    Automatic(bool),
    Sensitivity(u8),
    Scope(Scope),
    Record(Action),
    CancelRecord,
    Clear(Action),
    /// A binding was just recorded and accepted: persist and apply.
    Saved,
}

/// One raw key transition from the focused window.
#[derive(Clone, Debug)]
pub struct KeyInput {
    /// The W3C code name of the physical key.
    pub code: String,
    pub down: bool,
    /// Modifiers held at this moment, as [`mods`] bits.
    pub mods: u8,
}

/// What the OS listener subscription reports.
#[derive(Clone, Debug)]
pub enum GlobalEvent {
    Started,
    Failed(String),
    Event(ShortcutEvent),
    /// Events were dropped under load: release everything held.
    Lost,
}

/// What a key edge asks the application to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Do {
    /// A hold action (push to talk or push to mute) went down or up.
    Hold(Action, bool),
    /// A hold action was dropped without a release; release immediately.
    Cancel(Action),
    Toggle(Action),
    /// Recording ended with this binding; the caller decides whether it is accepted.
    Recorded(Action, Binding),
    /// Recording ended without a binding.
    RecordCancelled,
}

/// Transient shortcut state: the in-window matcher, the recorder and the listener's
/// diagnosis. Nothing here is persisted.
#[derive(Default)]
pub struct Runtime {
    pub recording: Option<Action>,
    /// After a binding was recorded on key-down, its key may still be held; that
    /// action ignores presses until the key is released.
    quiet: Option<(Action, Key)>,
    window: Option<Matcher>,
    /// Why the system-wide listener is not running, when it is not.
    pub global_error: Option<String>,
    pub global_running: bool,
    /// Why the last edit was refused.
    pub notice: Option<String>,
}

impl Runtime {
    /// Rebuilds the in-window matcher after the bindings changed.
    pub fn rebuild(&mut self, settings: &Settings) {
        self.window = Some(Matcher::new(&settings.list()));
    }

    fn route(&mut self, event: ShortcutEvent) -> Option<Do> {
        let action = Action::from_id(event.id)?;
        if self.recording.is_some() {
            return None;
        }
        match event.edge {
            Edge::Down => {
                if self.quiet.is_some_and(|(quiet, _)| quiet == action) {
                    return None;
                }
                Some(match action {
                    Action::PushToTalk | Action::PushToMute => Do::Hold(action, true),
                    Action::ToggleMute | Action::ToggleDeafen => Do::Toggle(action),
                })
            }
            Edge::Up => matches!(action, Action::PushToTalk | Action::PushToMute)
                .then_some(Do::Hold(action, false)),
            Edge::Cancel => matches!(action, Action::PushToTalk | Action::PushToMute)
                .then_some(Do::Cancel(action)),
        }
    }

    /// An event from the system-wide listener.
    pub fn global(&mut self, event: ShortcutEvent) -> Option<Do> {
        self.route(event)
    }

    /// A key transition in the focused window. While recording, it only feeds the
    /// recorder and can never trigger a call action.
    pub fn window_key(&mut self, scope: Scope, input: &KeyInput) -> Vec<Do> {
        let Some(key) = Key::from_code(&input.code) else {
            return Vec::new();
        };
        if self.quiet.is_some_and(|(_, held)| held == key) && !input.down {
            self.quiet = None;
        }
        if let Some(action) = self.recording {
            return self.record(action, key, input).into_iter().collect();
        }
        if scope != Scope::Window {
            return Vec::new();
        }
        let mut found = Vec::new();
        if let Some(matcher) = &mut self.window {
            matcher.key(key, input.down, &mut |event| found.push(event));
        }
        found
            .into_iter()
            .filter_map(|event| self.route(event))
            .collect()
    }

    fn record(&mut self, action: Action, key: Key, input: &KeyInput) -> Option<Do> {
        let binding = match (input.down, key.is_modifier()) {
            (true, false) => Binding {
                key,
                mods: input.mods & !key.modifier_class().unwrap_or(0),
            },
            // A modifier pressed and released alone is the binding itself.
            (false, true) => Binding { key, mods: 0 },
            _ => return None,
        };
        self.recording = None;
        self.quiet = (input.down).then_some((action, key));
        Some(Do::Recorded(action, binding))
    }

    /// Whether raw key events are needed to record or to finish a recording.
    pub fn wants_keys(&self) -> bool {
        self.recording.is_some() || self.quiet.is_some()
    }

    /// Escape while recording.
    pub fn cancel_record(&mut self) -> Option<Do> {
        self.recording.take().map(|_| Do::RecordCancelled)
    }

    /// The window lost focus: in-window keys are released, and a pending quiet ends.
    pub fn window_lost_focus(&mut self, scope: Scope) -> Vec<Do> {
        self.quiet = None;
        let mut found = Vec::new();
        if let Some(matcher) = &mut self.window {
            matcher.cancel(&mut |event| found.push(event));
        }
        if scope != Scope::Window {
            return Vec::new();
        }
        found
            .into_iter()
            .filter_map(|event| self.route(event))
            .collect()
    }
}

/// Maps an iced key event to a [`KeyInput`], when its physical key can be bound.
pub fn key_input(
    physical: iced::keyboard::key::Physical,
    modifiers: iced::keyboard::Modifiers,
    down: bool,
) -> Option<KeyInput> {
    let iced::keyboard::key::Physical::Code(code) = physical else {
        return None;
    };
    let mut held = 0;
    for (on, bit) in [
        (modifiers.control(), mods::CTRL),
        (modifiers.shift(), mods::SHIFT),
        (modifiers.alt(), mods::ALT),
        (modifiers.logo(), mods::SUPER),
    ] {
        if on {
            held |= bit;
        }
    }
    Some(KeyInput {
        code: format!("{code:?}"),
        down,
        mods: held,
    })
}

/// Identity of the system-wide listener subscription: it restarts when the bindings
/// change and exists only while the caller subscribes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListenerSpec(pub Vec<(u8, Binding)>);

impl Hash for ListenerSpec {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

/// Queue between the hook thread and the application, in events (a handful of bytes each).
const LISTENER_QUEUE: usize = 64;

pub fn listener(spec: &ListenerSpec) -> impl iced::futures::Stream<Item = GlobalEvent> + use<> {
    let bindings = spec.0.clone();
    iced::stream::channel(8, async move |mut output| {
        use iced::futures::SinkExt;
        let (tx, mut rx) = tokio::sync::mpsc::channel(LISTENER_QUEUE);
        let lost = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&lost);
        let started = GlobalShortcuts::start(&bindings, move |event| {
            if tx.try_send(event).is_err() {
                flag.store(true, Ordering::Release);
            }
        });
        let _listener = match started {
            Ok(listener) => listener,
            Err(error) => {
                let _ = output.send(GlobalEvent::Failed(error.to_string())).await;
                std::future::pending().await
            }
        };
        let _ = output.send(GlobalEvent::Started).await;
        while let Some(event) = rx.recv().await {
            if lost.swap(false, Ordering::AcqRel) {
                let _ = output.send(GlobalEvent::Lost).await;
            }
            let _ = output.send(GlobalEvent::Event(event)).await;
        }
    })
}

/// The section of the audio dialog for voice input.
pub fn view<'a>(
    settings: &Settings,
    runtime: &'a Runtime,
    vad_forbidden: bool,
    call_active: bool,
) -> Element<'a, crate::Message> {
    let message = crate::Message::VoiceInput;
    let manual = settings.sensitivity;
    let mut content = column![
        text(tr("Input mode")).size(18),
        pick_list(InputMode::ALL, Some(settings.mode), move |mode| message(
            Event::Mode(mode)
        ))
        .width(Length::Fill),
        text(if vad_forbidden {
            tr("This channel does not allow voice activity, so push to talk is used here.")
        } else {
            ""
        }),
    ]
    .spacing(8);
    if let Some(problem) = settings.problem() {
        content = content.push(text(problem));
    }
    match settings.mode {
        InputMode::VoiceActivity => {
            content = content
                .push(
                    checkbox(manual.is_none())
                        .label(tr("Automatic sensitivity (transmit everything the noise suppression lets through)"))
                        .on_toggle(move |automatic| message(Event::Automatic(automatic))),
                )
                .push(match manual {
                    Some(level) => Element::from(row![
                        slider(SENSITIVITY_RANGE, level, move |n| message(Event::Sensitivity(n))),
                        text(trf("Speech quieter than -{} dBFS is not sent", &[&level])),
                    ].spacing(12)),
                    None => text(tr("Manual sensitivity is off.")).into(),
                });
        }
        InputMode::PushToTalk => {
            content = content.push(
                row![
                    slider(0..=MAX_RELEASE_MS, settings.release_ms, move |ms| {
                        message(Event::ReleaseMs(ms - ms % 50))
                    }),
                    text(trf(
                        "Keep transmitting {} ms after release",
                        &[&settings.release_ms],
                    )),
                ]
                .spacing(12),
            );
        }
    }
    content = content
        .push(text(tr("Shortcuts")).size(18))
        .push(pick_list(Scope::ALL, Some(settings.scope), move |scope| {
            message(Event::Scope(scope))
        }))
        .push(text(scope_status(settings, runtime, call_active)));
    for action in Action::ALL {
        let recording = runtime.recording == Some(action);
        content = content.push(
            row![
                text(action.label()).width(Length::Fixed(140.0)),
                text(if recording {
                    tr("Press a key or combination… (Esc cancels)").to_owned()
                } else {
                    settings
                        .binding(action)
                        .map_or_else(|| tr("Not set").to_owned(), Binding::label)
                })
                .width(Length::Fill),
                button(tr(if recording { "Cancel" } else { "Set key" })).on_press(message(
                    if recording {
                        Event::CancelRecord
                    } else {
                        Event::Record(action)
                    }
                )),
                button(tr("Clear")).on_press_maybe(
                    settings
                        .binding(action)
                        .map(|_| message(Event::Clear(action)))
                ),
            ]
            .spacing(8),
        );
    }
    content = content
        .push(text(tr(runtime.notice.as_deref().unwrap_or(""))))
        .push(text(tr(
            "Shortcuts are heard only while you are in a call, and no other key is read or stored. The pressed key is still delivered to the focused application, so prefer a function key, a mouse-free key such as Right Ctrl, or a combination.",
        )));
    content.into()
}

fn scope_status(settings: &Settings, runtime: &Runtime, call_active: bool) -> String {
    match settings.scope {
        Scope::Window => {
            tr("In-window shortcuts work on every system. Pressing a key in another app is not seen, and leaving this window releases every held key.").to_owned()
        }
        Scope::Global => match (&runtime.global_error, global_support()) {
            (Some(error), _) => trf("System-wide shortcuts are inactive: {}", &[&tr(error)]),
            (None, Err(error)) => trf("System-wide shortcuts are inactive: {}", &[&tr(&error.to_string())]),
            (None, Ok(())) if runtime.global_running && call_active => {
                tr("System-wide listener running for this call.").to_owned()
            }
            (None, Ok(())) => tr("The system-wide listener starts when you join a call.").to_owned(),
        },
    }
}

/// Whether a call needs any shortcut listener at all.
pub fn wanted(settings: &Settings, call_active: bool) -> bool {
    call_active && !settings.list().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(text: &str) -> Binding {
        Binding::parse(text).unwrap()
    }

    fn input(code: &str, down: bool, mods: u8) -> KeyInput {
        KeyInput {
            code: code.to_owned(),
            down,
            mods,
        }
    }

    fn settings() -> Settings {
        let mut settings = Settings::default();
        settings
            .assign(Action::PushToTalk, binding("KeyV"))
            .unwrap();
        settings
            .assign(Action::ToggleMute, binding("Ctrl+KeyM"))
            .unwrap();
        settings
    }

    #[test]
    fn a_binding_cannot_serve_two_actions() {
        let mut settings = settings();
        assert_eq!(
            settings.assign(Action::PushToMute, binding("KeyV")),
            Err(Action::PushToTalk)
        );
        assert_eq!(settings.binding(Action::PushToMute), None);
        // Re-assigning the same action is not a conflict.
        assert_eq!(settings.assign(Action::PushToTalk, binding("KeyV")), Ok(()));
    }

    #[test]
    fn settings_round_trip_and_a_hand_edited_duplicate_is_dropped() {
        let mut original = settings();
        original.mode = InputMode::PushToTalk;
        original.release_ms = 350;
        original.sensitivity = Some(42);
        original.scope = Scope::Global;
        assert_eq!(Settings::from_json(&original.to_json()), original);

        let mut edited = original.to_json();
        edited["bindings"]["push_to_mute"] = json!("KeyV");
        edited["release_ms"] = json!(99_999);
        edited["sensitivity"] = json!(500);
        let loaded = Settings::from_json(&edited);
        assert_eq!(loaded.binding(Action::PushToMute), None);
        assert_eq!(loaded.release_ms, MAX_RELEASE_MS);
        assert_eq!(loaded.sensitivity, Some(100));
        assert_eq!(Settings::from_json(&json!("junk")), Settings::default());
    }

    #[test]
    fn window_keys_hold_toggle_and_lose_focus_without_a_stale_press() {
        let mut runtime = Runtime::default();
        let settings = settings();
        runtime.rebuild(&settings);
        let ptt = |down| input("KeyV", down, 0);
        assert_eq!(
            runtime.window_key(Scope::Window, &ptt(true)),
            [Do::Hold(Action::PushToTalk, true)]
        );
        // Auto-repeat is one press.
        assert!(runtime.window_key(Scope::Window, &ptt(true)).is_empty());
        assert_eq!(
            runtime.window_lost_focus(Scope::Window),
            [Do::Cancel(Action::PushToTalk)]
        );
        // A release that arrives after the cancel changes nothing.
        assert!(runtime.window_key(Scope::Window, &ptt(false)).is_empty());
        let toggle = runtime.window_key(Scope::Window, &input("ControlLeft", true, mods::CTRL));
        assert!(toggle.is_empty());
        assert_eq!(
            runtime.window_key(Scope::Window, &input("KeyM", true, mods::CTRL)),
            [Do::Toggle(Action::ToggleMute)]
        );
    }

    #[test]
    fn the_window_path_is_silent_for_system_wide_scope() {
        let mut runtime = Runtime::default();
        runtime.rebuild(&settings());
        assert!(
            runtime
                .window_key(Scope::Global, &input("KeyV", true, 0))
                .is_empty()
        );
    }

    #[test]
    fn recording_never_triggers_a_call_action() {
        let mut runtime = Runtime::default();
        let settings = settings();
        runtime.rebuild(&settings);
        runtime.recording = Some(Action::PushToMute);
        // The key an existing action uses is just recorded.
        assert_eq!(
            runtime.window_key(Scope::Window, &input("KeyV", true, 0)),
            [Do::Recorded(Action::PushToMute, binding("KeyV"))]
        );
        assert_eq!(runtime.recording, None);
        // Global events while recording are ignored.
        runtime.recording = Some(Action::PushToMute);
        let event = ShortcutEvent {
            id: Action::PushToTalk.id(),
            edge: Edge::Down,
        };
        assert_eq!(runtime.global(event), None);
        assert_eq!(runtime.cancel_record(), Some(Do::RecordCancelled));
    }

    #[test]
    fn the_recorded_key_stays_quiet_until_it_is_released() {
        let mut runtime = Runtime::default();
        runtime.rebuild(&Settings::default());
        runtime.recording = Some(Action::PushToTalk);
        runtime.window_key(Scope::Window, &input("KeyV", true, 0));
        let down = ShortcutEvent {
            id: Action::PushToTalk.id(),
            edge: Edge::Down,
        };
        assert_eq!(runtime.global(down), None, "held key repeating");
        runtime.window_key(Scope::Window, &input("KeyV", false, 0));
        assert_eq!(
            runtime.global(down),
            Some(Do::Hold(Action::PushToTalk, true))
        );
    }

    #[test]
    fn recorder_takes_chords_and_lone_modifiers() {
        let mut runtime = Runtime {
            recording: Some(Action::ToggleDeafen),
            ..Runtime::default()
        };
        // The modifier going down alone does not end recording.
        assert!(
            runtime
                .window_key(Scope::Window, &input("ControlLeft", true, mods::CTRL))
                .is_empty()
        );
        assert_eq!(
            runtime.window_key(Scope::Window, &input("ShiftLeft", false, 0)),
            [Do::Recorded(Action::ToggleDeafen, binding("ShiftLeft"))]
        );
        runtime.recording = Some(Action::ToggleDeafen);
        assert_eq!(
            runtime.window_key(
                Scope::Window,
                &input("KeyD", true, mods::CTRL | mods::SHIFT)
            ),
            [Do::Recorded(
                Action::ToggleDeafen,
                binding("Ctrl+Shift+KeyD")
            )]
        );
    }

    #[test]
    fn global_edges_map_to_holds_toggles_and_cancels() {
        let mut runtime = Runtime::default();
        let event = |action: Action, edge| ShortcutEvent {
            id: action.id(),
            edge,
        };
        assert_eq!(
            runtime.global(event(Action::PushToMute, Edge::Down)),
            Some(Do::Hold(Action::PushToMute, true))
        );
        assert_eq!(
            runtime.global(event(Action::PushToMute, Edge::Cancel)),
            Some(Do::Cancel(Action::PushToMute))
        );
        assert_eq!(
            runtime.global(event(Action::ToggleDeafen, Edge::Down)),
            Some(Do::Toggle(Action::ToggleDeafen))
        );
        assert_eq!(runtime.global(event(Action::ToggleDeafen, Edge::Up)), None);
    }

    #[test]
    fn listeners_exist_only_for_a_call_with_bindings() {
        assert!(!wanted(&Settings::default(), true));
        assert!(!wanted(&settings(), false));
        assert!(wanted(&settings(), true));
    }
}
