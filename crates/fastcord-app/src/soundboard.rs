//! Free server-local soundboard (issue #87): browse, search, favourite, preview and
//! explicitly play the open server's sounds in the voice channel the account is in, and
//! hear the sounds other participants play, with their own volume and a mute switch.
//!
//! * **Preview** is local: the sound is fetched from the public CDN through the shared
//!   credential-free cache, decoded off the UI thread (`fastcord_audio::soundboard`) and
//!   played on its own output-only engine. It sends nothing.
//! * **Play** is one explicit click: `POST /channels/{id}/send-soundboard-sound`, re-checked
//!   against the call and the `USE_SOUNDBOARD` + `SPEAK` permissions at the moment of the
//!   click, sent once and never retried. Discord stays authoritative.
//! * **Hearing others**: the official client plays a sound locally when the
//!   `VOICE_CHANNEL_EFFECT_SEND` event arrives (the sound is not mixed into the voice
//!   stream), so each cue starts one local player, at most [`MAX_REMOTE`] at a time, one per
//!   sound (a repeat restarts it). Nothing starts while deafened or while the soundboard is
//!   muted, and everything stops on leaving the call, deafening, muting, a closed panel
//!   (preview) or a logout.
//!
//! Budget: at most 64 sounds listed, 64 favourites, [`MAX_REMOTE`] + 1 players, each a clip
//! of at most 6 s (about 100 KiB of Opus packets) read from a file of at most 1 MiB.
//! Players exist only while a sound plays; the 250 ms tick runs only then.

use std::sync::Arc;

use fastcord_audio::{DeviceChoice, FilePlayer, PlayerState};
use fastcord_discord::{RestClient, RestError, SoundboardSound};
use fastcord_model::Snowflake;
use iced::widget::{button, column, container, row, scrollable, slider, text, text_input};
use iced::{Element, Length, Task};
use serde_json::{Value, json};

use crate::attachments::AttachmentCache;
use crate::gateway::SoundCue;
use crate::i18n::{tr, trf};

/// Sounds from other participants playing at once.
pub const MAX_REMOTE: usize = 3;
/// Favourites kept.
pub const MAX_FAVORITES: usize = 64;
const MAX_QUERY_CHARS: usize = 32;

/// The saved soundboard choices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefs {
    /// Received soundboard volume, 0 to 100 (independent of the voice volumes).
    pub volume: u8,
    /// Sounds from the call are not played.
    pub muted: bool,
    pub favorites: Vec<u64>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            volume: 100,
            muted: false,
            favorites: Vec::new(),
        }
    }
}

impl Prefs {
    pub fn from_json(value: &Value) -> Self {
        let mut favorites: Vec<u64> = value
            .get("favorites")
            .and_then(Value::as_array)
            .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
            .unwrap_or_default();
        favorites.dedup();
        favorites.truncate(MAX_FAVORITES);
        Self {
            volume: value
                .get("volume")
                .and_then(Value::as_u64)
                .map_or(100, |v| v.min(100) as u8),
            muted: value.get("muted").and_then(Value::as_bool) == Some(true),
            favorites,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"volume": self.volume, "muted": self.muted, "favorites": self.favorites})
    }

    /// The linear gain of a sound whose own volume is `percent`.
    fn gain(&self, percent: u8) -> f32 {
        f32::from(self.volume) / 100.0 * f32::from(percent.min(100)) / 100.0
    }

    fn is_favorite(&self, id: Snowflake) -> bool {
        self.favorites.contains(&id.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Preview,
    /// Played by this participant of the call.
    Call(Snowflake),
}

/// A started player on its way to the reducer.
#[derive(Clone)]
pub struct Handle(pub(crate) Arc<FilePlayer>);

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handle")
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Context),
    Close,
    Query(String),
    FavoritesOnly(bool),
    Favorite(Snowflake),
    Listed(u64, Result<Vec<SoundboardSound>, RestError>),
    Preview(Snowflake),
    Play(Snowflake),
    Sent(u64, Snowflake, Result<(), RestError>),
    /// A participant played a sound (from the Gateway).
    Cue(SoundCue),
    /// The server's sounds changed: list again if the panel shows that server.
    CatalogChanged(Snowflake),
    Loaded(u64, Result<Handle, &'static str>),
    Volume(u8),
    ToggleMute,
    Tick,
}

/// What the panel is opened for: the voice channel of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Context {
    pub guild: Snowflake,
    pub channel: Snowflake,
    /// The account has `USE_SOUNDBOARD` and `SPEAK` there.
    pub can_play: bool,
}

/// What the owner of the state lends to one update.
pub struct Env<'a> {
    pub client: &'a RestClient,
    pub cache: Option<&'a AttachmentCache>,
    pub output: DeviceChoice,
    pub deafened: bool,
    /// The voice channel the account is in: its server (none for a call) and ID.
    pub voice: Option<(Option<Snowflake>, Snowflake)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Loading,
    Failed(&'static str),
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Play {
    Idle,
    Sending(Snowflake),
    Sent(Snowflake),
    Failed(&'static str),
}

struct Pending {
    ticket: u64,
    origin: Origin,
    sound: Snowflake,
    percent: u8,
    /// Dropping aborts the fetch; a decode already running finishes and is ignored.
    _abort: iced::task::Handle,
}

struct Active {
    origin: Origin,
    sound: Snowflake,
    percent: u8,
    player: Arc<FilePlayer>,
}

pub struct State {
    open: Option<Context>,
    serial: u64,
    ticket: u64,
    seen: u64,
    seen_change: u64,
    status: Status,
    play: Play,
    sounds: Vec<SoundboardSound>,
    query: String,
    favorites_only: bool,
    prefs: Prefs,
    dirty: bool,
    pending: Vec<Pending>,
    active: Vec<Active>,
    /// Why the last sound could not be played.
    problem: Option<&'static str>,
    /// Dropping this aborts the list request and a play in flight.
    task: Option<iced::task::Handle>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            open: None,
            serial: 0,
            ticket: 0,
            seen: 0,
            seen_change: 0,
            status: Status::Loading,
            play: Play::Idle,
            sounds: Vec::new(),
            query: String::new(),
            favorites_only: false,
            prefs: Prefs::default(),
            dirty: false,
            pending: Vec::new(),
            active: Vec::new(),
            problem: None,
            task: None,
        }
    }
}

fn describe_list(error: &RestError) -> &'static str {
    match error {
        RestError::PermissionDenied | RestError::AuthenticationRequired => {
            "Discord did not let this account list the sounds here."
        }
        RestError::ResourceGone => "This server's sounds are no longer available.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting this request. Try again shortly."
        }
        _ => "The sounds could not be loaded. Try again.",
    }
}

fn describe_play(error: &RestError) -> &'static str {
    match error {
        RestError::PermissionDenied => {
            "Discord did not let this account play that sound here (permission, or the call has ended)."
        }
        RestError::ResourceGone => "That sound or voice channel no longer exists.",
        RestError::InvalidRateLimit
        | RestError::RateLimited
        | RestError::SchedulerCapacityExceeded => {
            "Discord is rate limiting sounds. Wait a moment, then click again."
        }
        RestError::WriteNotVerified(_) => fastcord_discord::WRITE_NOT_VERIFIED,
        _ => "The sound may have played. It is not sent again automatically.",
    }
}

fn describe_sound(error: fastcord_audio::soundboard::SoundError) -> &'static str {
    use fastcord_audio::soundboard::SoundError;
    match error {
        SoundError::TooLarge | SoundError::TooLong => "That sound is too long to play here.",
        SoundError::Unsupported => "The audio format of that sound is not supported.",
        SoundError::Damaged => "That sound file is damaged.",
    }
}

/// Fetches, decodes and starts one sound. Runs off the UI thread; the blocking part holds
/// at most the 1 MiB file and the decoded clip.
pub(crate) async fn start(
    cache: AttachmentCache,
    sound: Snowflake,
    output: DeviceChoice,
    gain: f32,
) -> Result<Handle, &'static str> {
    let path = cache
        .soundboard_file(sound)
        .await
        .map_err(|_| "That sound could not be downloaded.")?;
    tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(path).map_err(|_| "That sound could not be read.")?;
        play(&bytes, output, gain)
    })
    .await
    .map_err(|_| "Playback was cancelled.")?
}

/// Decodes and starts sound bytes. Blocking: call it on a worker thread.
fn play(bytes: &[u8], output: DeviceChoice, gain: f32) -> Result<Handle, &'static str> {
    let clip = fastcord_audio::soundboard::decode(bytes).map_err(describe_sound)?;
    let player = FilePlayer::start_source(clip, Some(output))
        .map_err(|_| "The output device could not play the sound.")?;
    player.set_volume(gain);
    Ok(Handle(Arc::new(player)))
}

/// The preview of a sound file chosen for upload, played from memory on its own
/// output-only engine like any #87 preview. Runs off the UI thread.
pub(crate) async fn start_bytes(
    bytes: Vec<u8>,
    output: DeviceChoice,
    gain: f32,
) -> Result<Handle, &'static str> {
    tokio::task::spawn_blocking(move || play(&bytes, output, gain))
        .await
        .map_err(|_| "Playback was cancelled.")?
}

impl State {
    /// Adopts the saved choices (after the preferences were read).
    pub fn set_prefs(&mut self, prefs: Prefs) {
        self.prefs = prefs;
    }

    /// The choices when they changed since the last call, to be saved.
    pub fn take_dirty_prefs(&mut self) -> Option<Prefs> {
        std::mem::take(&mut self.dirty).then(|| self.prefs.clone())
    }

    /// The server of a catalog change not seen before.
    pub fn new_catalog_change(&mut self, change: Option<(Snowflake, u64)>) -> Option<Snowflake> {
        let (guild, seq) = change.filter(|&(_, seq)| seq > self.seen_change)?;
        self.seen_change = seq;
        Some(guild)
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// Whether a sound is loading or playing, so the tick runs.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty() || !self.active.is_empty()
    }

    /// Forgets everything and stops every sound; the saved choices stay.
    pub fn reset(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ticket: self.ticket,
            seen: self.seen,
            seen_change: self.seen_change,
            prefs: std::mem::take(&mut self.prefs),
            ..Self::default()
        };
    }

    /// Follows the call: leaving it ends the panel and every sound, another channel ends
    /// the panel, and deafening silences everything.
    pub fn sync_call(&mut self, voice: Option<(Option<Snowflake>, Snowflake)>, deafened: bool) {
        match voice {
            None => self.reset(),
            Some((_, channel)) => {
                if self.open.is_some_and(|open| open.channel != channel) {
                    self.close_panel();
                }
                if deafened {
                    self.stop_all();
                }
            }
        }
    }

    fn close_panel(&mut self) {
        self.open = None;
        self.serial = self.serial.wrapping_add(1);
        self.status = Status::Loading;
        self.play = Play::Idle;
        self.sounds = Vec::new();
        self.query = String::new();
        self.favorites_only = false;
        self.task = None;
        self.problem = None;
        self.drop_origin(|origin| origin == Origin::Preview);
    }

    fn stop_all(&mut self) {
        self.pending.clear();
        self.active.clear();
    }

    fn drop_origin(&mut self, matches: impl Fn(Origin) -> bool) {
        self.pending.retain(|p| !matches(p.origin));
        self.active.retain(|a| !matches(a.origin));
    }

    fn remote_count(&self) -> usize {
        self.pending
            .iter()
            .filter(|p| p.origin != Origin::Preview)
            .count()
            + self
                .active
                .iter()
                .filter(|a| a.origin != Origin::Preview)
                .count()
    }

    fn list(&mut self, guild: Snowflake, client: &RestClient) -> Task<Event> {
        self.serial = self.serial.wrapping_add(1);
        self.status = Status::Loading;
        let (client, serial) = (client.clone(), self.serial);
        let (task, handle) = Task::perform(
            async move { client.guild_soundboard_sounds(guild).await },
            move |result| Event::Listed(serial, result),
        )
        .abortable();
        self.task = Some(handle.abort_on_drop());
        task
    }

    fn load(&mut self, origin: Origin, sound: Snowflake, percent: u8, env: &Env) -> Task<Event> {
        let Some(cache) = env.cache.cloned() else {
            self.problem = Some("Sounds cannot be fetched right now.");
            return Task::none();
        };
        self.ticket = self.ticket.wrapping_add(1);
        let ticket = self.ticket;
        let (output, gain) = (env.output.clone(), self.prefs.gain(percent));
        let (task, handle) = Task::perform(start(cache, sound, output, gain), move |result| {
            Event::Loaded(ticket, result)
        })
        .abortable();
        self.pending.push(Pending {
            ticket,
            origin,
            sound,
            percent,
            _abort: handle.abort_on_drop(),
        });
        task
    }

    /// Whether the rejected request means the login is no longer accepted (401).
    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Listed(_, Err(RestError::AuthenticationRequired))
                | Event::Sent(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    pub fn update(&mut self, event: Event, env: &Env) -> Task<Event> {
        match event {
            Event::Open(context) => {
                // Only the channel of the call, and only for the server it belongs to.
                if env.voice != Some((Some(context.guild), context.channel)) {
                    return Task::none();
                }
                self.close_panel();
                self.open = Some(context);
                return self.list(context.guild, env.client);
            }
            Event::Close => self.close_panel(),
            Event::Query(query) => {
                self.query = query.chars().take(MAX_QUERY_CHARS).collect();
            }
            Event::FavoritesOnly(on) => self.favorites_only = on,
            Event::Favorite(id) => {
                if let Some(at) = self.prefs.favorites.iter().position(|&f| f == id.0) {
                    self.prefs.favorites.remove(at);
                    self.dirty = true;
                } else if self.prefs.favorites.len() < MAX_FAVORITES {
                    self.prefs.favorites.push(id.0);
                    self.dirty = true;
                }
            }
            Event::Listed(serial, result) if serial == self.serial && self.open.is_some() => {
                match result {
                    Ok(sounds) => {
                        self.status = Status::Done;
                        // A sound that disappeared is no longer previewed or selected.
                        self.drop_origin(|origin| origin == Origin::Preview);
                        if let Play::Sending(id) | Play::Sent(id) = self.play
                            && !sounds.iter().any(|s| s.id == id)
                        {
                            self.play = Play::Idle;
                        }
                        self.sounds = sounds;
                    }
                    Err(error) => self.status = Status::Failed(describe_list(&error)),
                }
            }
            Event::Preview(id) => {
                let Some(sound) = self.sounds.iter().find(|s| s.id == id && s.available) else {
                    return Task::none();
                };
                if env.deafened {
                    self.problem = Some("Undeafen to hear sounds.");
                    return Task::none();
                }
                let was_playing = self
                    .active
                    .iter()
                    .any(|a| a.origin == Origin::Preview && a.sound == id);
                let percent = sound.volume_percent;
                self.drop_origin(|origin| origin == Origin::Preview);
                self.problem = None;
                if !was_playing {
                    return self.load(Origin::Preview, id, percent, env);
                }
            }
            Event::Play(id) => {
                let Some(open) = self.open else {
                    return Task::none();
                };
                // Re-checked at the click: still in this call, allowed, sound still there.
                let in_call = env.voice == Some((Some(open.guild), open.channel));
                let known = self.sounds.iter().any(|s| s.id == id && s.available);
                if !open.can_play || !in_call || !known || matches!(self.play, Play::Sending(_)) {
                    return Task::none();
                }
                self.play = Play::Sending(id);
                let (client, serial, channel) = (env.client.clone(), self.serial, open.channel);
                let (task, handle) = Task::perform(
                    async move { client.play_soundboard_sound(channel, id).await },
                    move |result| Event::Sent(serial, id, result),
                )
                .abortable();
                self.task = Some(handle.abort_on_drop());
                return task;
            }
            Event::Sent(serial, id, result) if serial == self.serial && self.open.is_some() => {
                self.play = match result {
                    Ok(()) => Play::Sent(id),
                    Err(error) => Play::Failed(describe_play(&error)),
                };
            }
            Event::CatalogChanged(guild) => {
                if self.open.is_some_and(|open| open.guild == guild) {
                    return self.list(guild, env.client);
                }
            }
            Event::Cue(cue) => {
                if cue.seq <= self.seen {
                    return Task::none();
                }
                self.seen = cue.seq;
                if self.prefs.muted || env.deafened || env.voice.is_none() {
                    return Task::none();
                }
                let origin = Origin::Call(cue.user);
                // A repeat of a sound restarts it instead of stacking.
                self.pending
                    .retain(|p| !(p.origin != Origin::Preview && p.sound == cue.sound));
                self.active
                    .retain(|a| !(a.origin != Origin::Preview && a.sound == cue.sound));
                while self.remote_count() >= MAX_REMOTE {
                    if let Some(at) = self.active.iter().position(|a| a.origin != Origin::Preview) {
                        self.active.remove(at);
                    } else if let Some(at) = self
                        .pending
                        .iter()
                        .position(|p| p.origin != Origin::Preview)
                    {
                        self.pending.remove(at);
                    } else {
                        break;
                    }
                }
                return self.load(origin, cue.sound, cue.volume_percent, env);
            }
            Event::Loaded(ticket, result) => {
                let Some(at) = self.pending.iter().position(|p| p.ticket == ticket) else {
                    // Superseded, muted or closed: dropping the handle stops the player.
                    return Task::none();
                };
                let pending = self.pending.remove(at);
                match result {
                    Ok(Handle(player)) => self.active.push(Active {
                        origin: pending.origin,
                        sound: pending.sound,
                        percent: pending.percent,
                        player,
                    }),
                    Err(reason) => self.problem = Some(reason),
                }
            }
            Event::Volume(volume) => {
                self.prefs.volume = volume.min(100);
                self.dirty = true;
                for active in &self.active {
                    active.player.set_volume(self.prefs.gain(active.percent));
                }
            }
            Event::ToggleMute => {
                self.prefs.muted = !self.prefs.muted;
                self.dirty = true;
                if self.prefs.muted {
                    self.drop_origin(|origin| origin != Origin::Preview);
                }
            }
            Event::Tick => self.active.retain(|a| {
                !matches!(
                    a.player.snapshot().state,
                    PlayerState::Ended | PlayerState::Failed(_)
                )
            }),
            // Answers of a closed panel or an older request.
            Event::Listed(..) | Event::Sent(..) => {}
        }
        Task::none()
    }

    fn visible(&self) -> Vec<&SoundboardSound> {
        let query = self.query.to_lowercase();
        let mut shown: Vec<&SoundboardSound> = self
            .sounds
            .iter()
            .filter(|s| query.is_empty() || s.name.to_lowercase().contains(&query))
            .filter(|s| !self.favorites_only || self.prefs.is_favorite(s.id))
            .collect();
        // Stable: favourites first, the name order of the list otherwise.
        shown.sort_by_key(|s| !self.prefs.is_favorite(s.id));
        shown
    }
}

pub fn view(state: &State) -> Option<Element<'_, Event>> {
    let open = state.open?;
    let note = |line: &'static str| text(tr(line)).size(12).style(text::secondary);
    let controls = row![
        text(tr("Soundboard of this server")).size(13),
        iced::widget::space::horizontal(),
        crate::icons::close(Some(Event::Close)),
    ]
    .align_y(iced::Alignment::Center);
    let listening = row![
        text(trf("Received volume {}%", &[&state.prefs.volume])).size(12),
        slider(0..=100u8, state.prefs.volume, Event::Volume).width(Length::Fixed(140.0)),
        button(
            text(if state.prefs.muted {
                tr("Soundboard muted")
            } else {
                tr("Mute soundboard")
            })
            .size(12)
        )
        .on_press(Event::ToggleMute),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    let filter = row![
        text_input(tr("Search sounds"), &state.query)
            .on_input(Event::Query)
            .size(13),
        button(
            text(if state.favorites_only {
                tr("Favorites only")
            } else {
                tr("All sounds")
            })
            .size(12)
        )
        .on_press(Event::FavoritesOnly(!state.favorites_only)),
    ]
    .spacing(8);
    let status: Element<'_, Event> = match (state.status, state.play, state.problem) {
        (Status::Failed(reason), ..) | (_, Play::Failed(reason), _) | (_, _, Some(reason)) => {
            text(tr(reason)).size(12).style(text::danger).into()
        }
        (Status::Loading, ..) => note("Loading…").into(),
        (_, Play::Sending(_), _) => note("Playing…").into(),
        (Status::Done, ..) if !open.can_play => {
            note("You may preview sounds, but not play them in this channel.").into()
        }
        (Status::Done, ..) if state.sounds.is_empty() => note("This server has no sounds.").into(),
        (Status::Done, ..) => {
            note("Preview is only for you. Play is heard by everyone in the call.").into()
        }
    };
    let rows = state.visible().into_iter().map(|sound| {
        let favorite = state.prefs.is_favorite(sound.id);
        let previewing = state
            .active
            .iter()
            .any(|a| a.origin == Origin::Preview && a.sound == sound.id);
        let mut label = sound.name.clone();
        if let Some(emoji) = &sound.emoji {
            label = format!("{emoji} {label}");
        }
        if !sound.available {
            label.push_str(tr(" (unavailable)"));
        }
        let play_enabled =
            open.can_play && sound.available && !matches!(state.play, Play::Sending(_));
        row![
            button(text(if favorite { "\u{2605}" } else { "\u{2606}" }).size(13))
                .on_press(Event::Favorite(sound.id))
                .style(button::text),
            text(label).size(13).width(Length::Fill),
            button(text(tr(if previewing { "Stop" } else { "Preview" })).size(12))
                .on_press_maybe(sound.available.then_some(Event::Preview(sound.id)))
                .style(button::secondary),
            button(text(tr("Play")).size(12))
                .on_press_maybe(play_enabled.then_some(Event::Play(sound.id)))
                .style(button::primary),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into()
    });
    Some(
        container(
            column![
                controls,
                listening,
                filter,
                status,
                scrollable(column(rows.collect::<Vec<Element<'_, Event>>>()).spacing(4))
                    .height(Length::Fixed(220.0)),
            ]
            .spacing(6),
        )
        .padding(8)
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            fastcord_net::NetworkContext::off(),
            UserToken::new("offline-soundboard-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn cache() -> AttachmentCache {
        let root = std::env::temp_dir().join(format!("fastcord-soundboard-{}", std::process::id()));
        AttachmentCache::new(
            &fastcord_net::NetworkContext::off(),
            Snowflake(1),
            root,
            "Mozilla/5.0 test",
        )
        .unwrap()
    }

    fn sound(id: u64, name: &str, available: bool) -> SoundboardSound {
        SoundboardSound {
            id: Snowflake(id),
            name: name.into(),
            volume_percent: 80,
            emoji: None,
            available,
            creator: None,
        }
    }

    const CALL: Option<(Option<Snowflake>, Snowflake)> = Some((Some(Snowflake(1)), Snowflake(5)));

    fn context(can_play: bool) -> Context {
        Context {
            guild: Snowflake(1),
            channel: Snowflake(5),
            can_play,
        }
    }

    fn open_state(can_play: bool) -> State {
        State {
            open: Some(context(can_play)),
            status: Status::Done,
            sounds: vec![sound(9, "bell", true), sound(10, "gone", false)],
            ..State::default()
        }
    }

    fn cue(seq: u64, sound: u64) -> SoundCue {
        SoundCue {
            seq,
            user: Snowflake(7),
            sound: Snowflake(sound),
            volume_percent: 100,
        }
    }

    #[test]
    fn play_is_rechecked_at_the_click_and_a_failure_is_never_retried() {
        let client = client();
        let env = |voice| Env {
            client: &client,
            cache: None,
            output: DeviceChoice::SystemDefault,
            deafened: false,
            voice,
        };
        // Denied, unavailable, unknown, or the call is another channel: nothing is sent.
        for (mut state, id, voice) in [
            (open_state(false), 9, CALL),
            (open_state(true), 10, CALL),
            (open_state(true), 11, CALL),
            (
                open_state(true),
                9,
                Some((Some(Snowflake(1)), Snowflake(6))),
            ),
            (open_state(true), 9, None),
        ] {
            let _ = state.update(Event::Play(Snowflake(id)), &env(voice));
            assert_eq!(state.play, Play::Idle, "{id} {voice:?}");
        }
        let mut state = open_state(true);
        let _ = state.update(Event::Play(Snowflake(9)), &env(CALL));
        assert_eq!(state.play, Play::Sending(Snowflake(9)));
        let _ = state.update(Event::Play(Snowflake(9)), &env(CALL));
        let serial = state.serial;
        let _ = state.update(Event::Sent(serial + 1, Snowflake(9), Ok(())), &env(CALL));
        assert_eq!(state.play, Play::Sending(Snowflake(9)), "stale answer");
        let _ = state.update(
            Event::Sent(serial, Snowflake(9), Err(RestError::InvalidJson)),
            &env(CALL),
        );
        assert_eq!(
            state.play,
            Play::Failed(describe_play(&RestError::InvalidJson))
        );
        assert!(describe_play(&RestError::InvalidJson).contains("not sent again"));
        assert!(State::is_rejected(&Event::Sent(
            0,
            Snowflake(1),
            Err(RestError::AuthenticationRequired)
        )));
    }

    #[test]
    fn a_catalog_change_drops_a_vanished_sound_and_leaving_the_call_ends_everything() {
        let client = client();
        let env = Env {
            client: &client,
            cache: None,
            output: DeviceChoice::SystemDefault,
            deafened: false,
            voice: CALL,
        };
        let mut state = open_state(true);
        state.play = Play::Sent(Snowflake(9));
        let _ = state.update(
            Event::Listed(state.serial, Ok(vec![sound(11, "new", true)])),
            &env,
        );
        assert_eq!(state.play, Play::Idle, "the old selection is invalidated");
        assert_eq!(state.sounds.len(), 1);
        state.prefs.volume = 40;
        state.sync_call(Some((Some(Snowflake(1)), Snowflake(6))), false);
        assert!(state.open.is_none(), "another channel closes the panel");
        let mut state = open_state(true);
        state.prefs.muted = true;
        state.sync_call(None, false);
        assert!(state.open.is_none() && state.sounds.is_empty());
        assert!(state.prefs.muted, "the saved choices survive");
    }

    #[test]
    fn heard_sounds_respect_mute_deafen_duplicates_and_the_player_limit() {
        let client = client();
        let cache = cache();
        let env = |deafened| Env {
            client: &client,
            cache: Some(&cache),
            output: DeviceChoice::SystemDefault,
            deafened,
            voice: CALL,
        };
        let mut state = State::default();
        let _ = state.update(Event::Cue(cue(1, 100)), &env(true));
        assert!(!state.busy(), "deafened: nothing starts");
        let _ = state.update(Event::Cue(cue(1, 100)), &env(false));
        assert!(!state.busy(), "a cue already seen is not played again");
        let _ = state.update(Event::Cue(cue(2, 100)), &env(false));
        assert_eq!(state.pending.len(), 1);
        let _ = state.update(Event::Cue(cue(3, 100)), &env(false));
        assert_eq!(state.pending.len(), 1, "a repeat restarts, never stacks");
        for (seq, id) in [(4, 101), (5, 102), (6, 103)] {
            let _ = state.update(Event::Cue(cue(seq, id)), &env(false));
        }
        assert_eq!(state.pending.len(), MAX_REMOTE);
        assert!(state.pending.iter().all(|p| p.sound != Snowflake(100)));
        let _ = state.update(Event::ToggleMute, &env(false));
        assert!(!state.busy() && state.prefs.muted);
        let _ = state.update(Event::Cue(cue(7, 104)), &env(false));
        assert!(!state.busy(), "muted: nothing starts");
        assert_eq!(state.take_dirty_prefs().map(|p| p.muted), Some(true));
        assert_eq!(state.take_dirty_prefs(), None);
        // A late answer of a dropped load is ignored.
        let _ = state.update(Event::Loaded(1, Err("x")), &env(false));
        assert_eq!(state.problem, None);
    }

    #[test]
    fn preview_sends_nothing_and_stops_when_deafened_or_closed() {
        let client = client();
        let cache = cache();
        let env = |deafened| Env {
            client: &client,
            cache: Some(&cache),
            output: DeviceChoice::SystemDefault,
            deafened,
            voice: CALL,
        };
        let mut state = open_state(false);
        let _ = state.update(Event::Preview(Snowflake(10)), &env(false));
        assert!(
            state.pending.is_empty(),
            "unavailable sounds do not preview"
        );
        let _ = state.update(Event::Preview(Snowflake(9)), &env(true));
        assert!(state.pending.is_empty() && state.problem.is_some());
        let _ = state.update(Event::Preview(Snowflake(9)), &env(false));
        assert_eq!(state.pending.len(), 1);
        assert_eq!(state.play, Play::Idle, "a preview never plays to the call");
        state.sync_call(CALL, true);
        assert!(!state.busy(), "deafening releases the preview");
        let _ = state.update(Event::Preview(Snowflake(9)), &env(false));
        let _ = state.update(Event::Close, &env(false));
        assert!(!state.busy(), "closing releases the preview");
    }

    #[test]
    fn prefs_round_trip_and_hostile_values_are_bounded() {
        let prefs = Prefs {
            volume: 35,
            muted: true,
            favorites: vec![3, 4],
        };
        assert_eq!(Prefs::from_json(&prefs.to_json()), prefs);
        let wild = json!({"volume": 9000, "favorites": (0..500).collect::<Vec<u64>>(), "muted": 1});
        let loaded = Prefs::from_json(&wild);
        assert_eq!(loaded.volume, 100);
        assert_eq!(loaded.favorites.len(), MAX_FAVORITES);
        assert!(!loaded.muted);
        assert_eq!(Prefs::from_json(&json!({})), Prefs::default());
        assert!((Prefs::default().gain(50) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn favourites_come_first_and_the_search_filters() {
        let mut state = open_state(true);
        state.sounds = vec![sound(1, "alpha", true), sound(2, "beta", true)];
        state.prefs.favorites = vec![2];
        let names: Vec<_> = state.visible().iter().map(|s| s.name.clone()).collect();
        assert_eq!(names, ["beta", "alpha"]);
        state.query = "AL".into();
        assert_eq!(state.visible().len(), 1);
        state.query.clear();
        state.favorites_only = true;
        assert_eq!(state.visible().len(), 1);
    }
}
