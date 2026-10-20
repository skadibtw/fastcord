//! Keyboard shortcuts for communication controls (push-to-talk, push-to-mute, mute and
//! deafen toggles): the portable key/binding model and press/release matcher
//! ([`Matcher`]), plus the OS-wide listener ([`GlobalShortcuts`]).
//!
//! The listener exists only while the caller holds it. On Windows it is one
//! low-level keyboard hook on a dedicated thread that blocks in the message loop
//! (no polling, no CPU while idle). The hook compares each key with the configured
//! bindings and forwards only a match; every other key is neither stored, logged nor
//! forwarded, and no key is ever swallowed. Other operating systems report
//! [`ShortcutError::Unsupported`] with the reason; the in-window path (the caller
//! feeds its own key events to a [`Matcher`]) works everywhere.

use std::fmt;

#[cfg(target_os = "windows")]
mod windows;

/// Longest binding list one matcher or listener handles.
pub const MAX_BINDINGS: usize = 16;

/// A physical key, identified by its Windows virtual-key code, which is the portable
/// identity used for storage (as a W3C code name, see [`Key::code`]) and matching.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Key(pub u16);

/// Named keys: W3C code, label, virtual-key code.
const NAMED: &[(&str, &str, u16)] = &[
    ("Space", "Space", 0x20),
    ("Tab", "Tab", 0x09),
    ("Enter", "Enter", 0x0D),
    ("Backspace", "Backspace", 0x08),
    ("CapsLock", "Caps Lock", 0x14),
    ("ScrollLock", "Scroll Lock", 0x91),
    ("NumLock", "Num Lock", 0x90),
    ("Pause", "Pause", 0x13),
    ("PrintScreen", "Print Screen", 0x2C),
    ("Insert", "Insert", 0x2D),
    ("Delete", "Delete", 0x2E),
    ("Home", "Home", 0x24),
    ("End", "End", 0x23),
    ("PageUp", "Page Up", 0x21),
    ("PageDown", "Page Down", 0x22),
    ("ArrowLeft", "Left", 0x25),
    ("ArrowUp", "Up", 0x26),
    ("ArrowRight", "Right", 0x27),
    ("ArrowDown", "Down", 0x28),
    ("Backquote", "`", 0xC0),
    ("Minus", "-", 0xBD),
    ("Equal", "=", 0xBB),
    ("BracketLeft", "[", 0xDB),
    ("BracketRight", "]", 0xDD),
    ("Backslash", "\\", 0xDC),
    ("Semicolon", ";", 0xBA),
    ("Quote", "'", 0xDE),
    ("Comma", ",", 0xBC),
    ("Period", ".", 0xBE),
    ("Slash", "/", 0xBF),
    ("NumpadMultiply", "Numpad *", 0x6A),
    ("NumpadAdd", "Numpad +", 0x6B),
    ("NumpadSubtract", "Numpad -", 0x6D),
    ("NumpadDecimal", "Numpad .", 0x6E),
    ("NumpadDivide", "Numpad /", 0x6F),
    ("ControlLeft", "Left Ctrl", 0xA2),
    ("ControlRight", "Right Ctrl", 0xA3),
    ("ShiftLeft", "Left Shift", 0xA0),
    ("ShiftRight", "Right Shift", 0xA1),
    ("AltLeft", "Left Alt", 0xA4),
    ("AltRight", "Right Alt", 0xA5),
    ("SuperLeft", "Left Win", 0x5B),
    ("SuperRight", "Right Win", 0x5C),
];

/// Modifier bits of a [`Binding`].
pub mod mods {
    pub const CTRL: u8 = 1;
    pub const SHIFT: u8 = 2;
    pub const ALT: u8 = 4;
    pub const SUPER: u8 = 8;
}

impl Key {
    /// The key a W3C code name (`KeyA`, `Digit1`, `F5`, `Numpad3`, `Space`,
    /// `ControlRight`, ...) names, or none for a key this build cannot bind.
    pub fn from_code(code: &str) -> Option<Self> {
        let number = |prefix: &str, low: u16, high: u16| {
            code.strip_prefix(prefix)
                .filter(|rest| !rest.starts_with('0') || rest.len() == 1)
                .and_then(|rest| rest.parse::<u16>().ok())
                .filter(|n| (low..=high).contains(n))
        };
        if let [b'K', b'e', b'y', letter @ b'A'..=b'Z'] = code.as_bytes() {
            return Some(Self(u16::from(*letter)));
        }
        if let Some(n) = number("Digit", 0, 9) {
            return Some(Self(0x30 + n));
        }
        if let Some(n) = number("F", 1, 24) {
            return Some(Self(0x6F + n));
        }
        if let Some(n) = number("Numpad", 0, 9) {
            return Some(Self(0x60 + n));
        }
        NAMED
            .iter()
            .find(|(name, ..)| *name == code)
            .map(|(.., vk)| Self(*vk))
    }

    /// The W3C code name, the inverse of [`from_code`](Self::from_code).
    pub fn code(self) -> String {
        match self.0 {
            0x41..=0x5A => format!("Key{}", char::from(self.0 as u8)),
            0x30..=0x39 => format!("Digit{}", self.0 - 0x30),
            0x70..=0x87 => format!("F{}", self.0 - 0x6F),
            0x60..=0x69 => format!("Numpad{}", self.0 - 0x60),
            vk => NAMED
                .iter()
                .find(|(.., v)| *v == vk)
                .map_or_else(String::new, |(name, ..)| (*name).to_owned()),
        }
    }

    /// What the user reads, for example `A`, `F5` or `Right Ctrl`.
    pub fn label(self) -> String {
        match self.0 {
            0x41..=0x5A => char::from(self.0 as u8).to_string(),
            0x30..=0x39 => (self.0 - 0x30).to_string(),
            0x70..=0x87 => format!("F{}", self.0 - 0x6F),
            0x60..=0x69 => format!("Numpad {}", self.0 - 0x60),
            vk => NAMED
                .iter()
                .find(|(.., v)| *v == vk)
                .map_or_else(|| format!("Key {vk}"), |(_, label, _)| (*label).to_owned()),
        }
    }

    /// This key's bit in the physical-modifier set (left and right are distinct), and
    /// its logical [`mods`] class.
    fn modifier(self) -> Option<(u8, u8)> {
        Some(match self.0 {
            0xA2 => (1, mods::CTRL),
            0xA3 => (2, mods::CTRL),
            0xA0 => (4, mods::SHIFT),
            0xA1 => (8, mods::SHIFT),
            0xA4 => (16, mods::ALT),
            0xA5 => (32, mods::ALT),
            0x5B => (64, mods::SUPER),
            0x5C => (128, mods::SUPER),
            _ => return None,
        })
    }

    /// Whether this is a Ctrl/Shift/Alt/Win key.
    pub fn is_modifier(self) -> bool {
        self.modifier().is_some()
    }

    /// The [`mods`] bit this key sets when it is a modifier.
    pub fn modifier_class(self) -> Option<u8> {
        self.modifier().map(|(_, class)| class)
    }
}

/// A key plus the modifiers that must be held with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Binding {
    pub key: Key,
    /// [`mods`] bits held with `key`; none for a modifier used as the key itself.
    pub mods: u8,
}

impl Binding {
    /// `Ctrl+Shift+KeyA` form, as stored. A modifier key alone has no prefix.
    pub fn to_code(self) -> String {
        let mut out = String::new();
        for (bit, name) in MOD_NAMES {
            if self.mods & bit != 0 {
                out.push_str(name);
                out.push('+');
            }
        }
        out + &self.key.code()
    }

    /// Parses [`to_code`](Self::to_code); anything unknown or malformed is rejected.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('+').collect::<Vec<_>>();
        let key = Key::from_code(parts.pop()?)?;
        let mut bits = 0;
        for part in parts {
            let (bit, _) = MOD_NAMES.iter().find(|(_, name)| *name == part)?;
            if bits & bit != 0 {
                return None;
            }
            bits |= bit;
        }
        // A modifier key is either the whole binding or, as the last key, never
        // combined with its own class.
        if key.modifier_class().is_some_and(|class| bits & class != 0) {
            return None;
        }
        Some(Self { key, mods: bits })
    }

    /// What the user reads, for example `Ctrl+Shift+A`.
    pub fn label(self) -> String {
        let mut out = String::new();
        for (bit, name) in MOD_NAMES {
            if self.mods & bit != 0 {
                out.push_str(name);
                out.push('+');
            }
        }
        out + &self.key.label()
    }
}

const MOD_NAMES: [(u8, &str); 4] = [
    (mods::CTRL, "Ctrl"),
    (mods::SHIFT, "Shift"),
    (mods::ALT, "Alt"),
    (mods::SUPER, "Super"),
];

/// What happened to a binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edge {
    /// The binding was pressed (auto-repeat does not produce another).
    Down,
    /// The binding was released.
    Up,
    /// The binding was dropped without a release: focus lost, listener stopped, or a
    /// release the OS never delivered. Callers release immediately, with no delay.
    Cancel,
}

/// A press or release of the binding with the caller's `id`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShortcutEvent {
    pub id: u8,
    pub edge: Edge,
}

/// Turns raw key transitions into binding presses and releases. Allocation-free after
/// construction, so it may run inside an OS hook.
#[derive(Clone, Debug)]
pub struct Matcher {
    bindings: [Option<(u8, Binding)>; MAX_BINDINGS],
    active: u16,
    /// Physical modifier keys currently down.
    physical: u8,
}

impl Matcher {
    /// At most [`MAX_BINDINGS`] bindings are used; ids need not be distinct from the
    /// binding order.
    pub fn new(bindings: &[(u8, Binding)]) -> Self {
        let mut slots = [None; MAX_BINDINGS];
        for (slot, binding) in slots.iter_mut().zip(bindings) {
            *slot = Some(*binding);
        }
        Self {
            bindings: slots,
            active: 0,
            physical: 0,
        }
    }

    /// The logical modifiers down, leaving out `except` (the key being judged).
    fn logical(&self, except: Option<Key>) -> u8 {
        let physical = self.physical & !except.and_then(Key::modifier).map_or(0, |(bit, _)| bit);
        [
            (0b0000_0011, mods::CTRL),
            (0b0000_1100, mods::SHIFT),
            (0b0011_0000, mods::ALT),
            (0b1100_0000, mods::SUPER),
        ]
        .iter()
        .filter(|(mask, _)| physical & mask != 0)
        .fold(0, |all, (_, class)| all | class)
    }

    /// Feeds one key transition. `emit` receives each binding that changed.
    pub fn key(&mut self, key: Key, down: bool, emit: &mut impl FnMut(ShortcutEvent)) {
        if down {
            let held = self.logical(Some(key));
            if let Some((bit, _)) = key.modifier() {
                self.physical |= bit;
            }
            for (index, slot) in self.bindings.iter().enumerate() {
                if let Some((id, binding)) = slot
                    && binding.key == key
                    && binding.mods == held
                    && self.active & (1 << index) == 0
                {
                    self.active |= 1 << index;
                    emit(ShortcutEvent {
                        id: *id,
                        edge: Edge::Down,
                    });
                }
            }
        } else {
            if let Some((bit, _)) = key.modifier() {
                self.physical &= !bit;
            }
            let held = self.logical(None);
            for (index, slot) in self.bindings.iter().enumerate() {
                if let Some((id, binding)) = slot
                    && self.active & (1 << index) != 0
                    && (binding.key == key || binding.mods & !held != 0)
                {
                    self.active &= !(1 << index);
                    emit(ShortcutEvent {
                        id: *id,
                        edge: Edge::Up,
                    });
                }
            }
        }
    }

    /// Drops every held binding (focus lost, listener ending) and forgets modifiers.
    pub fn cancel(&mut self, emit: &mut impl FnMut(ShortcutEvent)) {
        self.physical = 0;
        self.release_where(|_| true, emit);
    }

    /// Cancels held bindings whose key `is_down` no longer reports down: the recovery
    /// for a release the OS never delivered.
    pub fn release_stale(
        &mut self,
        is_down: impl Fn(Key) -> bool,
        emit: &mut impl FnMut(ShortcutEvent),
    ) {
        self.release_where(|binding| !is_down(binding.key), emit);
    }

    fn release_where(
        &mut self,
        stale: impl Fn(&Binding) -> bool,
        emit: &mut impl FnMut(ShortcutEvent),
    ) {
        for (index, slot) in self.bindings.iter().enumerate() {
            if let Some((id, binding)) = slot
                && self.active & (1 << index) != 0
                && stale(binding)
            {
                self.active &= !(1 << index);
                emit(ShortcutEvent {
                    id: *id,
                    edge: Edge::Cancel,
                });
            }
        }
    }

    /// Whether any binding is currently held.
    pub fn any_held(&self) -> bool {
        self.active != 0
    }
}

/// Why the global listener is not running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShortcutError {
    /// This operating system or session offers no way to observe keys while another
    /// window has focus; the text says why and what remains available.
    Unsupported(&'static str),
    /// The operating system refused to install the listener.
    Failed,
    /// More bindings than [`MAX_BINDINGS`].
    TooMany,
}

impl fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(reason) => f.write_str(reason),
            Self::Failed => {
                f.write_str("The operating system refused the global shortcut listener.")
            }
            Self::TooMany => f.write_str("Too many shortcuts."),
        }
    }
}

impl std::error::Error for ShortcutError {}

/// Whether this session can listen for keys system-wide; the error is the diagnosis
/// shown to the user.
pub fn global_support() -> Result<(), ShortcutError> {
    #[cfg(target_os = "windows")]
    {
        Ok(())
    }
    #[cfg(target_os = "macos")]
    {
        Err(ShortcutError::Unsupported(
            "Global shortcuts on macOS need an event tap with Accessibility permission, which this build does not implement yet. Use the in-window scope.",
        ))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
            || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland");
        Err(ShortcutError::Unsupported(if wayland {
            "Wayland does not let applications see keys while another window has focus, and this build does not implement the GlobalShortcuts portal. Use the in-window scope."
        } else {
            "Global shortcuts on X11 are not implemented in this build. Use the in-window scope."
        }))
    }
    #[cfg(not(any(target_os = "windows", unix)))]
    {
        Err(ShortcutError::Unsupported(
            "Global shortcuts are not supported on this platform.",
        ))
    }
}

/// The running OS-wide listener. Dropping it uninstalls the hook, joins its thread and
/// first reports [`Edge::Cancel`] for every held binding.
pub struct GlobalShortcuts {
    /// Held for its `Drop`, which uninstalls the hook.
    #[cfg(target_os = "windows")]
    #[allow(dead_code)]
    native: windows::Hook,
}

impl fmt::Debug for GlobalShortcuts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GlobalShortcuts")
    }
}

impl GlobalShortcuts {
    /// Installs the listener. `sink` runs on the listener thread for matches only and
    /// must return quickly (hand off with a bounded `try_send`).
    pub fn start(
        bindings: &[(u8, Binding)],
        sink: impl FnMut(ShortcutEvent) + Send + 'static,
    ) -> Result<Self, ShortcutError> {
        if bindings.len() > MAX_BINDINGS {
            return Err(ShortcutError::TooMany);
        }
        global_support()?;
        #[cfg(target_os = "windows")]
        {
            Ok(Self {
                native: windows::Hook::start(Matcher::new(bindings), Box::new(sink))?,
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = sink;
            Err(ShortcutError::Failed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: &str) -> Key {
        Key::from_code(code).unwrap()
    }

    fn binding(text: &str) -> Binding {
        Binding::parse(text).unwrap()
    }

    #[derive(Default)]
    struct Log(Vec<(u8, Edge)>);
    impl Log {
        fn sink(&mut self) -> impl FnMut(ShortcutEvent) + '_ {
            |e| self.0.push((e.id, e.edge))
        }
    }

    #[test]
    fn codes_round_trip_and_bad_ones_are_rejected() {
        for code in [
            "KeyA",
            "Digit7",
            "F1",
            "F24",
            "Numpad0",
            "Space",
            "ControlRight",
            "Backquote",
        ] {
            assert_eq!(key(code).code(), code);
        }
        for bad in [
            "", "KeyAB", "Key1", "F0", "F25", "F01", "Digit10", "Nope", "keya",
        ] {
            assert!(Key::from_code(bad).is_none(), "{bad}");
        }
        assert_eq!(binding("Ctrl+Shift+KeyA").label(), "Ctrl+Shift+A");
        assert_eq!(binding("Ctrl+Shift+KeyA").to_code(), "Ctrl+Shift+KeyA");
        assert!(Binding::parse("Shift+Ctrl+KeyA").is_some());
        assert!(Binding::parse("Ctrl+Ctrl+KeyA").is_none());
        assert!(Binding::parse("Ctrl+ControlLeft").is_none());
        assert!(Binding::parse("Hyper+KeyA").is_none());
    }

    #[test]
    fn press_release_and_autorepeat() {
        let mut matcher = Matcher::new(&[(1, binding("KeyV"))]);
        let mut log = Log::default();
        matcher.key(key("KeyV"), true, &mut log.sink());
        matcher.key(key("KeyV"), true, &mut log.sink());
        matcher.key(key("KeyV"), true, &mut log.sink());
        matcher.key(key("KeyB"), true, &mut log.sink());
        matcher.key(key("KeyV"), false, &mut log.sink());
        assert_eq!(log.0, [(1, Edge::Down), (1, Edge::Up)]);
        assert!(!matcher.any_held());
    }

    #[test]
    fn modifiers_must_match_exactly_and_releasing_one_releases_the_chord() {
        let mut matcher = Matcher::new(&[(1, binding("Ctrl+KeyM")), (2, binding("KeyM"))]);
        let mut log = Log::default();
        matcher.key(key("KeyM"), true, &mut log.sink());
        matcher.key(key("KeyM"), false, &mut log.sink());
        matcher.key(key("ControlLeft"), true, &mut log.sink());
        matcher.key(key("KeyM"), true, &mut log.sink());
        matcher.key(key("ControlLeft"), false, &mut log.sink());
        assert_eq!(
            log.0,
            [
                (2, Edge::Down),
                (2, Edge::Up),
                (1, Edge::Down),
                (1, Edge::Up)
            ]
        );
    }

    #[test]
    fn a_modifier_alone_can_be_the_binding() {
        let mut matcher = Matcher::new(&[(3, binding("ControlRight"))]);
        let mut log = Log::default();
        matcher.key(key("ControlLeft"), true, &mut log.sink());
        matcher.key(key("ControlRight"), true, &mut log.sink());
        assert!(
            log.0.is_empty(),
            "left Ctrl already down is a different chord"
        );
        matcher.key(key("ControlRight"), false, &mut log.sink());
        matcher.key(key("ControlLeft"), false, &mut log.sink());
        matcher.key(key("ControlRight"), true, &mut log.sink());
        matcher.key(key("ControlRight"), false, &mut log.sink());
        assert_eq!(log.0, [(3, Edge::Down), (3, Edge::Up)]);
    }

    #[test]
    fn cancel_and_stale_recovery_release_without_a_key_up() {
        let mut matcher = Matcher::new(&[(1, binding("KeyV")), (2, binding("KeyB"))]);
        let mut log = Log::default();
        matcher.key(key("KeyV"), true, &mut log.sink());
        matcher.key(key("KeyB"), true, &mut log.sink());
        matcher.release_stale(|k| k == key("KeyB"), &mut log.sink());
        assert_eq!(log.0.last(), Some(&(1, Edge::Cancel)));
        assert!(matcher.any_held());
        matcher.cancel(&mut log.sink());
        assert_eq!(log.0.last(), Some(&(2, Edge::Cancel)));
        assert!(!matcher.any_held());
        // A press after cancel works again.
        matcher.key(key("KeyV"), true, &mut log.sink());
        assert_eq!(log.0.last(), Some(&(1, Edge::Down)));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unsupported_systems_say_why() {
        let error = GlobalShortcuts::start(&[], |_| {}).unwrap_err();
        assert!(matches!(error, ShortcutError::Unsupported(text) if text.contains("in-window")));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn the_hook_installs_and_uninstalls_cleanly() {
        let held = GlobalShortcuts::start(&[(1, binding("F13"))], |_| {}).unwrap();
        drop(held);
    }
}
