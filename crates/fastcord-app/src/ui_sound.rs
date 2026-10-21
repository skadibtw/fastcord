//! Interface sounds: short, soft, original tones for mute, deafen, joining and leaving a
//! call, others joining and leaving it, push-to-talk, screen sharing and message pings.
//!
//! * The tones are synthesized here (sine or triangle notes with a fast attack and an
//!   exponential decay, at most 300 ms, about 56 KiB of f32 PCM at the largest), encoded to
//!   Opus packets by `fastcord_audio::soundboard::clip_from_mono` and played by the same
//!   output-only [`FilePlayer`] the soundboard uses, on the selected output device. They are
//!   never sent to the call. Nothing is kept between sounds: a player (and its device
//!   stream) exists only while a sound plays, at most [`MAX_PLAYING`] at once, so an idle
//!   app has no audio thread or stream. They are not part of the call engine's mixer, so the
//!   echo canceller does not see them; they are brief and quiet by default.
//! * Which sound an event plays is decided by [`Watcher`] from successive snapshots of the
//!   call (pure and clock-injected), so keyboard shortcuts, push-to-mute and the buttons all
//!   behave the same and the participants already in a call when it connects stay silent.
//! * While deafened, only the toggles themselves (and leaving) play: others joining or
//!   leaving, push-to-talk ticks and message pings are suppressed.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fastcord_audio::{DeviceChoice, FilePlayer, PlayerState};
use fastcord_model::Snowflake;

use crate::gateway::VoiceAudioState;

/// Sounds playing at once; further ones are dropped.
pub const MAX_PLAYING: usize = 3;
/// Participant changes this soon after the call connects are its initial list.
const JOIN_GRACE: Duration = Duration::from_millis(2500);
/// Default volume, percent.
pub const DEFAULT_VOLUME: u8 = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sound {
    Mute,
    Unmute,
    Deafen,
    Undeafen,
    /// You joined a voice channel.
    Joined,
    /// You left the call.
    Left,
    UserJoined,
    UserLeft,
    PttOn,
    PttOff,
    StreamStarted,
    StreamStopped,
    Ping,
}

impl Sound {
    #[cfg(test)]
    pub const ALL: [Sound; 13] = [
        Sound::Mute,
        Sound::Unmute,
        Sound::Deafen,
        Sound::Undeafen,
        Sound::Joined,
        Sound::Left,
        Sound::UserJoined,
        Sound::UserLeft,
        Sound::PttOn,
        Sound::PttOff,
        Sound::StreamStarted,
        Sound::StreamStopped,
        Sound::Ping,
    ];

    /// Sounds about other people or messages, silent while deafened.
    fn silenced_by_deafen(self) -> bool {
        matches!(
            self,
            Sound::UserJoined | Sound::UserLeft | Sound::Ping | Sound::PttOn | Sound::PttOff
        )
    }
}

/// Whether `sound` may play at all.
pub fn allowed(sound: Sound, enabled: bool, deafened: bool) -> bool {
    enabled && !(deafened && sound.silenced_by_deafen())
}

struct Note {
    hz: f32,
    start_ms: u32,
    len_ms: u32,
    amp: f32,
}

fn n(hz: f32, start_ms: u32, len_ms: u32, amp: f32) -> Note {
    Note {
        hz,
        start_ms,
        len_ms,
        amp,
    }
}

/// The notes of a sound and whether they are triangle waves (otherwise sine).
fn score(sound: Sound) -> (Vec<Note>, bool) {
    match sound {
        Sound::Mute => (vec![n(698.0, 0, 70, 0.6), n(523.0, 70, 110, 0.6)], false),
        Sound::Unmute => (vec![n(523.0, 0, 70, 0.6), n(698.0, 70, 110, 0.6)], false),
        Sound::Deafen => (
            vec![
                n(494.0, 0, 70, 0.6),
                n(392.0, 70, 70, 0.6),
                n(294.0, 140, 120, 0.6),
            ],
            true,
        ),
        Sound::Undeafen => (
            vec![
                n(294.0, 0, 70, 0.6),
                n(392.0, 70, 70, 0.6),
                n(494.0, 140, 120, 0.6),
            ],
            true,
        ),
        Sound::Joined => (
            vec![
                n(440.0, 0, 80, 0.55),
                n(587.0, 80, 80, 0.55),
                n(784.0, 160, 130, 0.55),
            ],
            false,
        ),
        Sound::Left => (
            vec![
                n(784.0, 0, 80, 0.55),
                n(587.0, 80, 80, 0.55),
                n(440.0, 160, 130, 0.55),
            ],
            false,
        ),
        Sound::UserJoined => (vec![n(659.0, 0, 60, 0.4), n(880.0, 60, 110, 0.4)], false),
        Sound::UserLeft => (vec![n(880.0, 0, 60, 0.4), n(659.0, 60, 110, 0.4)], false),
        Sound::PttOn => (vec![n(1200.0, 0, 25, 0.2)], false),
        Sound::PttOff => (vec![n(900.0, 0, 25, 0.2)], false),
        Sound::StreamStarted => (
            vec![
                n(392.0, 0, 70, 0.55),
                n(587.0, 70, 70, 0.55),
                n(880.0, 140, 140, 0.55),
            ],
            true,
        ),
        Sound::StreamStopped => (
            vec![
                n(880.0, 0, 70, 0.55),
                n(587.0, 70, 70, 0.55),
                n(392.0, 140, 140, 0.55),
            ],
            true,
        ),
        Sound::Ping => (vec![n(988.0, 0, 70, 0.5), n(1319.0, 90, 150, 0.5)], false),
    }
}

/// The sound as mono 48 kHz samples.
pub fn synthesize(sound: Sound) -> Vec<f32> {
    const RATE: f32 = 48_000.0;
    let (notes, triangle) = score(sound);
    let end = notes
        .iter()
        .map(|n| n.start_ms + n.len_ms)
        .max()
        .unwrap_or(0);
    let mut out = vec![0.0f32; (end as usize * 48).min(48 * 300)];
    for note in notes {
        let first = note.start_ms as usize * 48;
        let len = note.len_ms as usize * 48;
        for i in 0..len {
            let Some(slot) = out.get_mut(first + i) else {
                break;
            };
            let t = i as f32 / RATE;
            let phase = (t * note.hz).fract();
            let wave = if triangle {
                4.0 * (phase - 0.5).abs() - 1.0
            } else {
                (phase * std::f32::consts::TAU).sin()
            };
            let seconds = note.len_ms as f32 / 1000.0;
            let attack = (t / 0.004).min(1.0);
            let release = ((seconds - t) / 0.006).clamp(0.0, 1.0);
            *slot += wave * note.amp * attack * release * (-t * 3.0 / seconds).exp();
        }
    }
    out
}

/// What the call looks like to the sound logic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snap {
    pub state: VoiceAudioState,
    pub self_muted: bool,
    pub deafened: bool,
    pub push_to_mute: bool,
    /// Push-to-talk mode: whether the key is held.
    pub ptt: Option<bool>,
    pub sharing: bool,
    /// The other people in the call.
    pub users: Vec<Snowflake>,
}

/// Turns successive snapshots into sounds.
#[derive(Default)]
pub struct Watcher {
    prev: Option<Snap>,
    running_since: Option<Instant>,
}

impl Watcher {
    pub fn step(&mut self, snap: Snap, now: Instant) -> Vec<Sound> {
        let Some(prev) = self.prev.replace(snap.clone()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let was = prev.state == VoiceAudioState::Running;
        let is = snap.state == VoiceAudioState::Running;
        let left = was && snap.state == VoiceAudioState::Idle;
        if !was && is {
            self.running_since = Some(now);
            out.push(Sound::Joined);
        }
        if left {
            self.running_since = None;
            out.push(Sound::Left);
        }
        if was && is {
            if snap.deafened != prev.deafened {
                out.push(if snap.deafened {
                    Sound::Deafen
                } else {
                    Sound::Undeafen
                });
            } else if snap.self_muted != prev.self_muted {
                out.push(if snap.self_muted {
                    Sound::Mute
                } else {
                    Sound::Unmute
                });
            }
            if snap.push_to_mute != prev.push_to_mute {
                out.push(if snap.push_to_mute {
                    Sound::Mute
                } else {
                    Sound::Unmute
                });
            }
            if let (Some(down), Some(was_down)) = (snap.ptt, prev.ptt)
                && down != was_down
            {
                out.push(if down { Sound::PttOn } else { Sound::PttOff });
            }
            let settled = self
                .running_since
                .is_some_and(|since| now.duration_since(since) >= JOIN_GRACE);
            if settled {
                if snap.users.iter().any(|u| !prev.users.contains(u)) {
                    out.push(Sound::UserJoined);
                }
                if prev.users.iter().any(|u| !snap.users.contains(u)) {
                    out.push(Sound::UserLeft);
                }
            }
        }
        if snap.sharing != prev.sharing && !left {
            out.push(if snap.sharing {
                Sound::StreamStarted
            } else {
                Sound::StreamStopped
            });
        }
        out
    }
}

static ENABLED: AtomicBool = AtomicBool::new(true);
static VOLUME: AtomicU8 = AtomicU8::new(DEFAULT_VOLUME);
static DEAFENED: AtomicBool = AtomicBool::new(false);
static PLAYING: AtomicUsize = AtomicUsize::new(0);
static OUTPUT: Mutex<Option<DeviceChoice>> = Mutex::new(None);
static WATCHER: Mutex<Option<Watcher>> = Mutex::new(None);

/// Adopts the saved choices (also after each change).
pub fn configure(enabled: bool, volume: u8, output: &DeviceChoice) {
    ENABLED.store(enabled, Ordering::Relaxed);
    VOLUME.store(volume.min(100), Ordering::Relaxed);
    if let Ok(mut slot) = OUTPUT.lock() {
        *slot = Some(output.clone());
    }
}

/// Looks at the call after a Gateway notification and plays what changed.
pub fn observe(snap: Snap) {
    DEAFENED.store(
        snap.deafened && snap.state == VoiceAudioState::Running,
        Ordering::Relaxed,
    );
    let sounds = WATCHER
        .lock()
        .map(|mut watcher| {
            watcher
                .get_or_insert_with(Watcher::default)
                .step(snap, Instant::now())
        })
        .unwrap_or_default();
    for sound in sounds {
        play(sound);
    }
}

/// Plays `sound` now if the settings and the call allow it. Never blocks.
pub fn play(sound: Sound) {
    if !allowed(
        sound,
        ENABLED.load(Ordering::Relaxed),
        DEAFENED.load(Ordering::Relaxed),
    ) {
        return;
    }
    let gain = f32::from(VOLUME.load(Ordering::Relaxed)) / 100.0;
    if gain <= 0.0 {
        return;
    }
    if PLAYING.fetch_add(1, Ordering::AcqRel) >= MAX_PLAYING {
        PLAYING.fetch_sub(1, Ordering::AcqRel);
        return;
    }
    let output = OUTPUT
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or(DeviceChoice::SystemDefault);
    let spawned = std::thread::Builder::new()
        .name("fastcord-ui-sound".to_owned())
        .spawn(move || {
            play_blocking(sound, gain, output);
            PLAYING.fetch_sub(1, Ordering::AcqRel);
        });
    if spawned.is_err() {
        PLAYING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Synthesizes, plays and waits for one sound; the output stream is gone when it returns.
fn play_blocking(sound: Sound, gain: f32, output: DeviceChoice) {
    let Ok(clip) = fastcord_audio::soundboard::clip_from_mono(&synthesize(sound)) else {
        return;
    };
    let Ok(player) = FilePlayer::start_source(clip, Some(output)) else {
        return;
    };
    player.set_volume(gain.clamp(0.0, 1.0));
    let deadline = Instant::now() + Duration::from_secs(2);
    while player.snapshot().state == PlayerState::Playing && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(30));
    }
    // Let the device drain what was queued before the stream is dropped.
    std::thread::sleep(Duration::from_millis(200));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(state: VoiceAudioState) -> Snap {
        Snap {
            state,
            self_muted: false,
            deafened: false,
            push_to_mute: false,
            ptt: None,
            sharing: false,
            users: Vec::new(),
        }
    }

    fn running() -> Snap {
        snap(VoiceAudioState::Running)
    }

    #[test]
    fn every_sound_is_short_quiet_and_not_silent() {
        for sound in Sound::ALL {
            let pcm = synthesize(sound);
            // Synthesized on demand and dropped: one sound is at most 57.6 KB of f32.
            assert!(pcm.len() * 4 <= 60 * 1024);
            assert!(pcm.len() <= 48 * 300, "{sound:?} {}", pcm.len());
            assert!(pcm.len() >= 48 * 20, "{sound:?}");
            let peak = pcm.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            assert!((0.05..=0.8).contains(&peak), "{sound:?} {peak}");
            assert!(pcm.iter().all(|s| s.is_finite()));
            assert!(
                pcm[0].abs() < 0.05 && pcm.last().unwrap().abs() < 0.05,
                "{sound:?}"
            );
        }
    }

    #[test]
    fn every_sound_encodes_to_a_clip_of_its_length() {
        for sound in Sound::ALL {
            let pcm = synthesize(sound);
            let clip = fastcord_audio::soundboard::clip_from_mono(&pcm).unwrap();
            assert!(
                clip.len_ms().abs_diff(pcm.len() as u64 / 48) <= 20,
                "{sound:?}"
            );
        }
    }

    #[test]
    fn deafening_silences_others_but_not_the_toggles() {
        assert!(!allowed(Sound::Ping, true, true));
        assert!(!allowed(Sound::UserJoined, true, true));
        assert!(!allowed(Sound::UserLeft, true, true));
        assert!(allowed(Sound::Undeafen, true, true));
        assert!(allowed(Sound::Mute, true, true));
        assert!(allowed(Sound::Ping, true, false));
        assert!(
            !allowed(Sound::Mute, false, false),
            "disabled plays nothing"
        );
    }

    #[test]
    fn joining_and_leaving_the_call() {
        let t = Instant::now();
        let mut w = Watcher::default();
        assert!(
            w.step(snap(VoiceAudioState::Idle), t).is_empty(),
            "baseline"
        );
        assert!(w.step(snap(VoiceAudioState::Connecting), t).is_empty());
        assert_eq!(w.step(running(), t), [Sound::Joined]);
        assert_eq!(w.step(snap(VoiceAudioState::Idle), t), [Sound::Left]);
    }

    #[test]
    fn toggles_play_only_inside_a_call_and_deafen_replaces_mute() {
        let t = Instant::now();
        let mut w = Watcher::default();
        w.step(snap(VoiceAudioState::Idle), t);
        let mut muted_idle = snap(VoiceAudioState::Idle);
        muted_idle.self_muted = true;
        assert!(w.step(muted_idle, t).is_empty());
        w.step(running(), t);
        let mut muted = running();
        muted.self_muted = true;
        assert_eq!(w.step(muted.clone(), t), [Sound::Mute]);
        muted.self_muted = false;
        assert_eq!(w.step(muted.clone(), t), [Sound::Unmute]);
        // Deafening also mutes: one sound.
        muted.self_muted = true;
        muted.deafened = true;
        assert_eq!(w.step(muted.clone(), t), [Sound::Deafen]);
        muted.deafened = false;
        assert_eq!(w.step(muted, t), [Sound::Undeafen]);
        let mut pushed = running();
        pushed.self_muted = true;
        pushed.push_to_mute = true;
        assert_eq!(w.step(pushed, t), [Sound::Mute]);
    }

    #[test]
    fn push_to_talk_only_in_that_mode_and_share_edges() {
        let t = Instant::now();
        let mut w = Watcher::default();
        w.step(running(), t);
        let mut shared = running();
        shared.sharing = true;
        assert_eq!(w.step(shared.clone(), t), [Sound::StreamStarted]);
        shared.sharing = false;
        assert_eq!(w.step(shared, t), [Sound::StreamStopped]);
        let mut a = running();
        a.ptt = Some(false);
        assert!(
            w.step(a.clone(), t).is_empty(),
            "entering the mode is silent"
        );
        a.ptt = Some(true);
        assert_eq!(w.step(a.clone(), t), [Sound::PttOn]);
        a.ptt = Some(false);
        assert_eq!(w.step(a, t), [Sound::PttOff]);
        // A share ending because the call ended is just the leave sound.
        let mut sharing = running();
        sharing.sharing = true;
        w.step(sharing, t);
        assert_eq!(w.step(snap(VoiceAudioState::Idle), t), [Sound::Left]);
    }

    #[test]
    fn the_initial_participant_list_is_silent_and_later_changes_play_once() {
        let t = Instant::now();
        let mut w = Watcher::default();
        w.step(snap(VoiceAudioState::Connecting), t);
        w.step(running(), t);
        let mut s = running();
        s.users = vec![Snowflake(1), Snowflake(2), Snowflake(3)];
        assert!(w.step(s.clone(), t + Duration::from_millis(300)).is_empty());
        let later = t + Duration::from_secs(5);
        s.users.push(Snowflake(4));
        s.users.push(Snowflake(5));
        assert_eq!(
            w.step(s.clone(), later),
            [Sound::UserJoined],
            "a burst is one sound"
        );
        s.users.remove(0);
        assert_eq!(w.step(s, later), [Sound::UserLeft]);
    }

    #[test]
    #[ignore = "plays every sound on the default output device"]
    fn hardware_plays_every_sound() {
        for sound in Sound::ALL {
            eprintln!("{sound:?}");
            play_blocking(sound, 0.4, DeviceChoice::SystemDefault);
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}
