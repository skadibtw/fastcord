//! Explicit voice-message recording (issue #75): press Record, see the recording state, Stop,
//! preview, then Send or Discard. Nothing is ever sent without pressing Send.
//!
//! This is a compatibility-gated extension: Discord documents recording and sending voice
//! messages for its mobile apps only, and the user-account route used here (the ordinary
//! cloud-upload slot, then a message with the voice-message flag, duration and waveform on the
//! one attachment) is taken from the public bot documentation and is not verified against a
//! user account. The panel therefore exists only when the user turns on "Voice message
//! recording (experimental)" in the audio settings, and it says so. Discord's refusal is shown
//! as it is; the program does not imitate another client or send the audio as an ordinary file.
//!
//! The microphone is open only while the phase is Recording (and Starting, until the device
//! has opened). Stop, Discard, a channel change, logout, a length limit or a device failure
//! all end it; the recorder is dropped off the UI thread. Opening, finishing (joining the
//! recorder, writing the file) and preview playback also run off the UI thread.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fastcord_audio::recorder::MAX_RECORDING_MS;
use fastcord_audio::{
    DeviceChoice, FilePlayer, NoiseSuppression, PlayerState, RecorderSnapshot, Recording,
    RecordingFailure, VoiceRecorder,
};
use fastcord_discord::UploadFile;
use fastcord_model::Snowflake;

use crate::i18n::{tr, trf};
use iced::widget::{button, column, container, progress_bar, row, text};
use iced::{Element, Task};
use parking_lot::Mutex;

const DIR: &str = "fastcord-voice";
const PREVIEW_FAILED: &str = "The preview could not be played. Check the output device.";
const SAVE_FAILED: &str = "The recording could not be saved.";
const EXPERIMENTAL: &str = "Experimental: Discord documents voice-message recording for mobile only. Whether it is accepted from this client is unverified.";

/// A value whose destructor may block (joining an audio thread) is dropped on a short-lived
/// thread, never on the UI thread.
pub(crate) struct Detached<T: Send + 'static>(pub(crate) Option<T>);

impl<T: Send + 'static> Drop for Detached<T> {
    fn drop(&mut self) {
        if let Some(value) = self.0.take() {
            let _ = std::thread::Builder::new()
                .name("fastcord-release".to_owned())
                .spawn(move || drop(value));
        }
    }
}

/// A running recorder on its way to the reducer.
#[derive(Clone)]
pub struct RecorderHandle(Arc<Mutex<Detached<VoiceRecorder>>>);

impl std::fmt::Debug for RecorderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RecorderHandle")
    }
}

impl RecorderHandle {
    fn snapshot(&self) -> Option<RecorderSnapshot> {
        self.0.lock().0.as_ref().map(VoiceRecorder::snapshot)
    }

    fn take(&self) -> Option<VoiceRecorder> {
        self.0.lock().0.take()
    }
}

/// A started preview player.
#[derive(Clone)]
pub struct PreviewHandle(Arc<Detached<FilePlayer>>);

impl std::fmt::Debug for PreviewHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreviewHandle")
    }
}

impl PreviewHandle {
    fn state(&self) -> Option<PlayerState> {
        self.0.0.as_ref().map(|player| player.snapshot().state)
    }
}

/// A finished recording saved as the file that will be uploaded.
#[derive(Clone, Debug)]
pub struct Ready {
    file: UploadFile,
    path: PathBuf,
    duration_ms: u64,
    bars: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Start,
    Started(u64, Result<RecorderHandle, RecordingFailure>),
    Tick,
    Stop,
    Finished(u64, Result<Ready, RecordingFailure>),
    TogglePreview,
    PreviewReady(u64, Result<PreviewHandle, &'static str>),
    Send,
    Cancel,
}

/// What the panel needs from the rest of the program.
pub struct Context {
    pub channel: Snowflake,
    pub can_send: bool,
    /// The user turned the experimental feature on.
    pub enabled: bool,
    pub input: DeviceChoice,
    pub output: DeviceChoice,
    pub noise_suppression: NoiseSuppression,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Starting,
    Recording,
    Finishing,
    Ready,
}

pub struct State {
    serial: u64,
    channel: Option<Snowflake>,
    phase: Phase,
    notice: Option<String>,
    recorder: Option<RecorderHandle>,
    snapshot: Option<RecorderSnapshot>,
    ready: Option<Ready>,
    preview: Option<PreviewHandle>,
    previewing: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            serial: 0,
            channel: None,
            phase: Phase::Idle,
            notice: None,
            recorder: None,
            snapshot: None,
            ready: None,
            preview: None,
            previewing: false,
        }
    }
}

fn directory() -> PathBuf {
    std::env::temp_dir().join(DIR)
}

/// Removes recordings an earlier run left behind (older than twelve hours, so a second
/// running instance keeps its own).
pub fn purge_stale() {
    let Ok(entries) = std::fs::read_dir(directory()) else {
        return;
    };
    let limit = std::time::Duration::from_secs(12 * 60 * 60);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > limit);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Writes a finished recording where the upload will read it, as `voice-message.ogg` (the
/// name the official clients use) in a directory of its own. Blocking.
fn prepare(recording: Recording) -> Result<Ready, RecordingFailure> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = directory().join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|_| RecordingFailure::Internal)?;
    let path = dir.join("voice-message.ogg");
    std::fs::write(&path, &recording.ogg).map_err(|_| RecordingFailure::Internal)?;
    let file = UploadFile::inspect_temporary(&path)
        .map_err(|_| RecordingFailure::Internal)?
        .into_voice_message(recording.duration_ms, &recording.waveform);
    Ok(Ready {
        file,
        path,
        duration_ms: recording.duration_ms,
        bars: crate::media_ui::group_bars(recording.waveform),
    })
}

fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

impl State {
    /// Discards everything for a channel other than `channel` (the microphone included).
    pub fn sync(&mut self, channel: Option<Snowflake>) {
        if self.channel.is_some() && self.channel != channel {
            self.reset();
        }
    }

    /// Forgets everything: stops the microphone and the preview, and ignores late results.
    pub fn reset(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    /// Whether the 100 ms timer is needed: a recording or a playing preview.
    pub fn ticking(&self) -> bool {
        self.phase == Phase::Recording || self.previewing
    }

    /// Whether the microphone is, or is being, opened.
    #[cfg(test)]
    fn microphone_open(&self) -> bool {
        matches!(self.phase, Phase::Starting | Phase::Recording)
    }

    /// The file went into the send queue: the draft is done.
    pub fn sent(&mut self) {
        self.reset();
    }

    fn fail(&mut self, reason: String) {
        self.reset();
        self.notice = Some(reason);
    }

    /// Handles one event. A returned file is to be queued as a message of its own (empty
    /// text); the caller then reports [`State::sent`], or leaves the draft for another try.
    pub fn update(&mut self, event: Event, context: &Context) -> (Task<Event>, Option<UploadFile>) {
        let usable = context.enabled && context.can_send;
        match event {
            Event::Start if usable && self.phase == Phase::Idle => {
                self.reset();
                self.phase = Phase::Starting;
                self.channel = Some(context.channel);
                let (serial, input, noise) = (
                    self.serial,
                    context.input.clone(),
                    context.noise_suppression,
                );
                return (
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                VoiceRecorder::start(input, noise).map(|recorder| {
                                    RecorderHandle(Arc::new(Mutex::new(Detached(Some(recorder)))))
                                })
                            })
                            .await
                            .unwrap_or(Err(RecordingFailure::Internal))
                        },
                        move |result| Event::Started(serial, result),
                    ),
                    None,
                );
            }
            Event::Started(serial, result)
                if serial == self.serial && self.phase == Phase::Starting =>
            {
                match result {
                    Ok(handle) => {
                        self.snapshot = handle.snapshot();
                        self.recorder = Some(handle);
                        self.phase = Phase::Recording;
                    }
                    Err(failure) => self.fail(failure.to_string()),
                }
            }
            Event::Tick => {
                if self
                    .channel
                    .is_some_and(|channel| channel != context.channel)
                {
                    self.reset();
                    return (Task::none(), None);
                }
                if let Some(state) = self.preview.as_ref().and_then(PreviewHandle::state)
                    && matches!(state, PlayerState::Ended | PlayerState::Failed(_))
                {
                    if matches!(state, PlayerState::Failed(_)) {
                        self.notice = Some(tr(PREVIEW_FAILED).to_owned());
                    }
                    self.preview = None;
                    self.previewing = false;
                }
                if self.phase == Phase::Recording {
                    self.snapshot = self.recorder.as_ref().and_then(RecorderHandle::snapshot);
                    match self.snapshot.and_then(|snapshot| snapshot.stopped) {
                        // The length limit: keep what was recorded.
                        Some(Ok(())) => return self.update(Event::Stop, context),
                        Some(Err(failure)) => self.fail(failure.to_string()),
                        None => {}
                    }
                }
            }
            Event::Stop if self.phase == Phase::Recording => {
                let Some(recorder) = self.recorder.take().and_then(|handle| handle.take()) else {
                    self.fail(RecordingFailure::Internal.to_string());
                    return (Task::none(), None);
                };
                self.phase = Phase::Finishing;
                let serial = self.serial;
                return (
                    Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || prepare(recorder.finish()?))
                                .await
                                .unwrap_or(Err(RecordingFailure::Internal))
                        },
                        move |result| Event::Finished(serial, result),
                    ),
                    None,
                );
            }
            Event::Finished(serial, result)
                if serial == self.serial && self.phase == Phase::Finishing =>
            {
                match result {
                    Ok(ready) => {
                        self.ready = Some(ready);
                        self.phase = Phase::Ready;
                    }
                    Err(RecordingFailure::Internal) => self.fail(tr(SAVE_FAILED).to_owned()),
                    Err(failure) => self.fail(failure.to_string()),
                }
            }
            Event::TogglePreview if self.phase == Phase::Ready => {
                if self.previewing {
                    self.preview = None;
                    self.previewing = false;
                } else if let Some(ready) = &self.ready {
                    self.previewing = true;
                    let (serial, path, output) =
                        (self.serial, ready.path.clone(), context.output.clone());
                    return (
                        Task::perform(
                            async move {
                                tokio::task::spawn_blocking(move || {
                                    FilePlayer::start(&path, output).map(|player| {
                                        PreviewHandle(Arc::new(Detached(Some(player))))
                                    })
                                })
                                .await
                                .ok()
                                .and_then(Result::ok)
                                .ok_or_else(|| tr(PREVIEW_FAILED))
                            },
                            move |result| Event::PreviewReady(serial, result),
                        ),
                        None,
                    );
                }
            }
            Event::PreviewReady(serial, result) if serial == self.serial => match result {
                Ok(handle) if self.previewing => self.preview = Some(handle),
                Ok(_) => {}
                Err(reason) => {
                    self.previewing = false;
                    self.notice = Some(reason.to_owned());
                }
            },
            Event::Send if self.phase == Phase::Ready && usable => {
                if self.channel != Some(context.channel) {
                    self.reset();
                    return (Task::none(), None);
                }
                self.preview = None;
                self.previewing = false;
                return (Task::none(), self.ready.as_ref().map(|r| r.file.clone()));
            }
            Event::Cancel => self.reset(),
            // Late results of a draft that was discarded, and presses that do not apply.
            _ => {}
        }
        (Task::none(), None)
    }
}

pub fn view<'a>(state: &'a State, context: &Context) -> Option<Element<'a, Event>> {
    if !context.enabled || !context.can_send {
        return None;
    }
    let small = |label: &'static str, event: Event| {
        button(text(tr(label)).size(12))
            .padding([2, 8])
            .on_press(event)
            .style(button::secondary)
    };
    // Recording starts from the "+" menu; idle, only a refusal is shown.
    if state.phase == Phase::Idle && state.notice.is_none() {
        return None;
    }
    let line = match state.phase {
        Phase::Idle => {
            let mut line = row![small("Record voice message", Event::Start)]
                .spacing(8)
                .align_y(iced::Alignment::Center);
            line = line.push(
                text(state.notice.as_deref().unwrap_or(tr(EXPERIMENTAL)))
                    .size(11)
                    .style(if state.notice.is_some() {
                        text::danger
                    } else {
                        text::secondary
                    }),
            );
            line
        }
        Phase::Starting => row![
            text(tr("Opening the microphone…")).size(12),
            small("Cancel", Event::Cancel)
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
        Phase::Recording => {
            let snapshot = state.snapshot.unwrap_or(RecorderSnapshot {
                elapsed_ms: 0,
                level: 0,
                stopped: None,
            });
            row![
                text(tr("Recording")).size(12).style(text::danger),
                text(format!(
                    "{} / {}",
                    clock(snapshot.elapsed_ms),
                    clock(MAX_RECORDING_MS)
                ))
                .size(12),
                progress_bar(0.0..=255.0, f32::from(snapshot.level))
                    .length(80)
                    .girth(6),
                small("Stop", Event::Stop),
                small("Cancel", Event::Cancel),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
        }
        Phase::Finishing => row![text(tr("Saving the recording…")).size(12)],
        Phase::Ready => {
            let ready = state.ready.as_ref()?;
            let mut line =
                row![text(trf("Voice message {}", &[&clock(ready.duration_ms)])).size(12)]
                    .spacing(8)
                    .align_y(iced::Alignment::Center);
            if !ready.bars.is_empty() {
                line = line.push(crate::media_ui::waveform_view::<Event>(&ready.bars, 1.0));
            }
            line = line
                .push(small(
                    if state.previewing {
                        "Stop preview"
                    } else {
                        "Preview"
                    },
                    Event::TogglePreview,
                ))
                .push(small("Discard", Event::Cancel))
                .push(
                    button(text(tr("Send")).size(12))
                        .padding([2, 8])
                        .on_press(Event::Send),
                );
            if let Some(notice) = &state.notice {
                line = line.push(text(tr(notice.as_str())).size(11).style(text::danger));
            }
            line
        }
    };
    Some(container(column![line]).padding([4, 8]).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            channel: Snowflake(5),
            can_send: true,
            enabled: true,
            input: DeviceChoice::SystemDefault,
            output: DeviceChoice::SystemDefault,
            noise_suppression: NoiseSuppression::Light,
        }
    }

    fn recording() -> Recording {
        Recording {
            ogg: vec![1, 2, 3, 4],
            duration_ms: 2_500,
            waveform: (0..=255).collect(),
        }
    }

    fn ready_state() -> State {
        State {
            channel: Some(Snowflake(5)),
            phase: Phase::Ready,
            ready: Some(prepare(recording()).unwrap()),
            ..State::default()
        }
    }

    #[test]
    fn nothing_starts_unless_enabled_sendable_and_idle() {
        for (enabled, can_send) in [(false, true), (true, false)] {
            let mut state = State::default();
            let mut ctx = context();
            ctx.enabled = enabled;
            ctx.can_send = can_send;
            let _ = state.update(Event::Start, &ctx);
            assert_eq!(state.phase, Phase::Idle, "{enabled} {can_send}");
            assert!(view(&state, &ctx).is_none());
        }
        // Presses that do not apply to the phase are ignored, never a panic or a send.
        let mut state = State::default();
        for event in [Event::Stop, Event::Send, Event::TogglePreview, Event::Tick] {
            let (_, file) = state.update(event, &context());
            assert!(file.is_none());
        }
        assert_eq!(state.phase, Phase::Idle);
        assert!(!state.ticking() && !state.microphone_open());
    }

    #[test]
    fn a_failed_start_is_said_and_a_stale_result_is_ignored() {
        let mut state = State::default();
        let ctx = context();
        let _ = state.update(Event::Start, &ctx);
        assert!(state.microphone_open());
        let serial = state.serial;
        let _ = state.update(
            Event::Started(serial, Err(RecordingFailure::PermissionDenied)),
            &ctx,
        );
        assert_eq!(state.phase, Phase::Idle);
        assert_eq!(
            state.notice.as_deref(),
            Some(RecordingFailure::PermissionDenied.to_string().as_str())
        );
        assert!(!state.microphone_open());
        // Discarded while starting: the late success changes nothing.
        let _ = state.update(Event::Start, &ctx);
        let stale = state.serial;
        let _ = state.update(Event::Cancel, &ctx);
        let _ = state.update(Event::Started(stale, Err(RecordingFailure::Device)), &ctx);
        assert_eq!((state.phase, state.notice.as_deref()), (Phase::Idle, None));
    }

    #[test]
    fn a_saved_recording_is_sent_once_as_a_voice_file_and_only_on_press() {
        let mut state = ready_state();
        let serial = state.serial;
        // A late finish for another draft does nothing.
        let _ = state.update(
            Event::Finished(serial + 1, Err(RecordingFailure::Device)),
            &context(),
        );
        assert_eq!(state.phase, Phase::Ready);
        let (_, file) = state.update(Event::Send, &context());
        let file = file.expect("Send queues the file");
        assert!(file.is_voice_message());
        assert_eq!(file.filename(), "voice-message.ogg");
        // Until the caller reports it queued, the draft stays (a full queue loses nothing).
        assert_eq!(state.phase, Phase::Ready);
        state.sent();
        assert_eq!(state.phase, Phase::Idle);
        assert!(state.ready.is_none());
        // Nothing but an explicit Send ever yields a file.
        let mut state = ready_state();
        for event in [Event::Tick, Event::Stop, Event::Cancel] {
            let (_, file) = state.update(event, &context());
            assert!(file.is_none());
        }
        assert_eq!(state.phase, Phase::Idle);
    }

    #[test]
    fn a_changed_channel_or_lost_permission_discards_the_draft() {
        let mut state = ready_state();
        state.sync(Some(Snowflake(5)));
        assert_eq!(state.phase, Phase::Ready);
        state.sync(Some(Snowflake(6)));
        assert_eq!(state.phase, Phase::Idle);
        let mut state = ready_state();
        let mut elsewhere = context();
        elsewhere.channel = Snowflake(6);
        let (_, file) = state.update(Event::Send, &elsewhere);
        assert!(file.is_none());
        assert_eq!(state.phase, Phase::Idle);
        let mut state = ready_state();
        let mut readonly = context();
        readonly.can_send = false;
        let (_, file) = state.update(Event::Send, &readonly);
        assert!(file.is_none());
    }

    #[test]
    fn the_recording_is_saved_under_the_official_name_and_removed_with_the_draft() {
        let state = ready_state();
        let ready = state.ready.clone().unwrap();
        assert!(ready.path.is_file());
        assert_eq!(ready.bars.len(), 64);
        assert_eq!(ready.duration_ms, 2_500);
        drop(state);
        assert!(ready.path.is_file(), "a clone of the file keeps it");
        let path = ready.path.clone();
        drop(ready);
        assert!(!path.exists(), "the last owner removes the recording");
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[test]
    fn the_panel_shows_each_phase_with_its_controls() {
        let ctx = context();
        let mut state = State::default();
        // Idle, recording starts from the message box's "+" menu.
        assert!(view(&state, &ctx).is_none());
        state.notice = Some("refused".to_owned());
        assert!(view(&state, &ctx).is_some());
        for phase in [Phase::Starting, Phase::Recording, Phase::Finishing] {
            state.phase = phase;
            assert!(view(&state, &ctx).is_some(), "{phase:?}");
        }
        let state = ready_state();
        assert!(view(&state, &ctx).is_some());
        assert_eq!(clock(61_000), "1:01");
    }
}
