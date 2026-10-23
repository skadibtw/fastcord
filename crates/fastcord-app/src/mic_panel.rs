//! Microphone gain and diagnostics in the audio settings (issue #83).
//!
//! The panel shows a live level meter, the gain controls, the echo canceller's state and two
//! checks that never touch a call: a level check (the meter alone) and a record-and-play-back
//! test. Both open the microphone only while they run and are dropped, off the UI thread, when
//! stopped, when the dialog closes or when a device fails.
//!
//! During a call the meter reads the running engine instead, through the shared voice
//! controls: the engine measures the level only while this dialog is open, and the app polls it
//! at 10 Hz on a timer that exists only while the dialog is open. Nothing here runs when the
//! dialog is closed.

use std::sync::Arc;

use fastcord_audio::{
    DeviceChoice, EchoDiagnostics, InputDiagnostics, InputGain, MAX_GAIN_DB, MIN_GAIN_DB, MicTest,
    MicTestFailure, MicTestMode, MicTestPhase, MicTestSnapshot, NoiseSuppression,
};
use iced::widget::{button, checkbox, column, progress_bar, row, slider, text};
use iced::{Element, Task};
use parking_lot::Mutex;

use crate::voice_record::Detached;

/// Quietest level the meter draws.
const FLOOR_DB: f32 = -60.0;
/// The peak marker falls by this much per 100 ms tick.
const PEAK_FALL_DB: f32 = 2.0;

/// A running check on its way to the reducer.
#[derive(Clone)]
pub struct Handle(Arc<Mutex<Detached<MicTest>>>);

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MicTestHandle")
    }
}

impl Handle {
    fn snapshot(&self) -> Option<MicTestSnapshot> {
        self.0.lock().0.as_ref().map(MicTest::snapshot)
    }

    fn set_gain(&self, gain: InputGain) {
        if let Some(test) = self.0.lock().0.as_ref() {
            test.set_gain(gain);
        }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    /// Gain slider moved (applied at once, saved on release).
    Gain(i8),
    GainReleased,
    Automatic(bool),
    Start(MicTestMode),
    Started(u64, Result<Handle, MicTestFailure>),
    Stop,
    Tick,
}

/// What the panel needs from the rest of the program.
pub struct Context {
    pub input: DeviceChoice,
    pub output: DeviceChoice,
    pub noise_suppression: NoiseSuppression,
    pub gain: InputGain,
    pub in_call: bool,
}

/// The measurement on screen.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Level {
    rms_db: f32,
    peak_db: f32,
    clipped: u64,
    silent: bool,
    /// The level of the call's microphone, not of a check.
    from_call: bool,
}

pub struct State {
    serial: u64,
    starting: bool,
    mode: Option<MicTestMode>,
    test: Option<Handle>,
    phase: Option<MicTestPhase>,
    level: Option<Level>,
    peak_hold: f32,
    echo: Option<EchoDiagnostics>,
    /// The call's microphone encoder target in bits/s.
    send_bitrate: Option<u32>,
    notice: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            serial: 0,
            starting: false,
            mode: None,
            test: None,
            phase: None,
            level: None,
            peak_hold: FLOOR_DB,
            echo: None,
            send_bitrate: None,
            notice: None,
        }
    }
}

impl State {
    /// Stops any check and forgets what was measured.
    pub fn close(&mut self) {
        self.serial += 1;
        *self = Self {
            serial: self.serial,
            ..Self::default()
        };
    }

    pub fn running(&self) -> bool {
        self.starting || self.test.is_some()
    }

    /// Applies a gain change to a running check at once.
    pub fn set_gain(&self, gain: InputGain) {
        if let Some(test) = &self.test {
            test.set_gain(gain);
        }
    }

    pub fn update(&mut self, event: Event, context: &Context) -> Task<Event> {
        match event {
            Event::Start(mode) if !context.in_call && !self.running() => {
                self.serial += 1;
                let serial = self.serial;
                self.starting = true;
                self.mode = Some(mode);
                self.notice = None;
                self.phase = None;
                let (input, output) = (context.input.clone(), context.output.clone());
                let (noise, gain) = (context.noise_suppression, context.gain);
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || {
                            MicTest::start(mode, input, output, noise, gain)
                                .map(|test| Handle(Arc::new(Mutex::new(Detached(Some(test))))))
                        })
                        .await
                        .unwrap_or(Err(MicTestFailure::Internal))
                    },
                    move |result| Event::Started(serial, result),
                );
            }
            Event::Started(serial, result) if serial == self.serial && self.starting => {
                self.starting = false;
                match result {
                    Ok(handle) => self.test = Some(handle),
                    Err(failure) => {
                        self.mode = None;
                        self.notice = Some(failure.to_string());
                    }
                }
            }
            // A start the dialog no longer wants: dropping the handle releases the devices.
            Event::Started(..) => {}
            Event::Stop => {
                self.serial += 1;
                self.starting = false;
                self.test = None;
                self.mode = None;
                self.phase = None;
                self.level = None;
            }
            Event::Gain(_) | Event::GainReleased | Event::Automatic(_) | Event::Start(_) => {}
            Event::Tick => {}
        }
        Task::none()
    }

    /// Reads the new measurement: the check's own while one runs, otherwise the call's.
    pub fn tick(&mut self, call: Option<InputDiagnostics>) {
        self.peak_hold = (self.peak_hold - PEAK_FALL_DB).max(FLOOR_DB);
        if let Some(handle) = &self.test {
            let Some(snapshot) = handle.snapshot() else {
                return;
            };
            self.echo = None;
            self.phase = Some(snapshot.phase);
            match snapshot.phase {
                MicTestPhase::Recording => {
                    self.show(Level {
                        rms_db: f32::from(snapshot.rms_tenths_db) / 10.0,
                        peak_db: f32::from(snapshot.peak_tenths_db) / 10.0,
                        clipped: snapshot.clipped_frames,
                        silent: snapshot.silent,
                        from_call: false,
                    });
                }
                MicTestPhase::Playing => self.level = None,
                MicTestPhase::Finished => {
                    self.test = None;
                    self.mode = None;
                    self.level = None;
                    self.notice = Some("Test finished.".to_owned());
                }
                MicTestPhase::Failed(failure) => {
                    self.test = None;
                    self.mode = None;
                    self.level = None;
                    self.notice = Some(failure.to_string());
                }
            }
        } else if let Some(call) = call {
            self.echo = call.echo;
            self.send_bitrate = (call.send_bitrate > 0).then_some(call.send_bitrate);
            if call.frames == 0 {
                self.level = None;
            } else {
                self.show(Level {
                    rms_db: call.rms_db(),
                    peak_db: call.peak_db(),
                    clipped: call.clipped_frames,
                    silent: false,
                    from_call: true,
                });
            }
        } else {
            self.level = None;
            self.echo = None;
            self.send_bitrate = None;
        }
    }

    fn show(&mut self, level: Level) {
        self.peak_hold = self.peak_hold.max(level.peak_db);
        self.level = Some(level);
    }
}

fn level_text(level: Option<Level>, peak_hold: f32) -> String {
    match level {
        None => "No level yet.".to_owned(),
        Some(level) => format!(
            "Level {:.0} dBFS, peak {:.0} dBFS{}",
            level.rms_db.max(FLOOR_DB - 40.0),
            peak_hold,
            if level.from_call {
                " (your microphone in this call)"
            } else {
                ""
            }
        ),
    }
}

fn echo_text(echo: Option<EchoDiagnostics>, in_call: bool) -> String {
    match echo {
        Some(echo) if echo.delay_ms > 0 => format!(
            "Echo cancellation: found an echo path of about {} ms, microphone reduced by {:.0} dB while others speak ({}).",
            echo.delay_ms,
            f32::from(echo.reduction_tenths_db) / 10.0,
            if echo.converged {
                "adapted"
            } else {
                "still adapting"
            }
        ),
        Some(_) => "Echo cancellation: running, no echo path found yet (it needs a few seconds of others speaking through your speakers).".to_owned(),
        None if in_call => "Echo cancellation: off or waiting for the first audio.".to_owned(),
        None => "Echo cancellation statistics appear during a call.".to_owned(),
    }
}

/// What the check is doing, in a sentence.
fn status_text(state: &State, in_call: bool) -> String {
    if let Some(notice) = &state.notice {
        return notice.clone();
    }
    if state.starting {
        return "Opening the microphone…".to_owned();
    }
    match (state.mode, state.phase) {
        (Some(MicTestMode::RecordAndPlay), Some(MicTestPhase::Playing)) => {
            "Playing back what you recorded…".to_owned()
        }
        (Some(MicTestMode::RecordAndPlay), _) => "Recording: speak now.".to_owned(),
        (Some(MicTestMode::Level), _) => {
            "Listening. The microphone stays open until you stop or close this window.".to_owned()
        }
        (None, _) if in_call => {
            "Leave the call to run the checks; the meter shows your call microphone.".to_owned()
        }
        (None, _) => String::new(),
    }
}

pub fn view<'a>(
    state: &'a State,
    gain: InputGain,
    in_call: bool,
    sensitivity: Option<u8>,
) -> Element<'a, crate::Message> {
    let wrap = crate::Message::MicPanel;
    let meter_value = state
        .level
        .map_or(FLOOR_DB, |level| level.rms_db)
        .max(FLOOR_DB);
    let mut parts: Vec<Element<'a, crate::Message>> = vec![
        text("Microphone").size(20).into(),
        text(format!("Input gain: {:+} dB", gain.manual_db)).into(),
        slider(
            i16::from(MIN_GAIN_DB)..=i16::from(MAX_GAIN_DB),
            i16::from(gain.manual_db),
            move |db| wrap(Event::Gain(db as i8)),
        )
        .on_release(wrap(Event::GainReleased))
        .into(),
        checkbox(gain.automatic)
            .label("Automatic gain")
            .on_toggle(move |on| wrap(Event::Automatic(on)))
            .into(),
        text("Gain is applied after noise suppression and before the voice-activity threshold, with a limiter so loud speech does not clip. Automatic gain steers speech toward a steady level and leaves silence alone.").into(),
        progress_bar(FLOOR_DB..=0.0, meter_value).into(),
        text(level_text(state.level, state.peak_hold)).into(),
    ];
    if let Some(level) = state.level {
        if level.clipped > 0 {
            parts.push(
                text(format!(
                    "{} frames reached full scale or the limiter ceiling. Lower the gain or move the microphone back.",
                    level.clipped
                ))
                .into(),
            );
        }
        if level.silent {
            parts.push(text(MicTestFailure::Silent.to_string()).into());
        }
        if let Some(n) = sensitivity {
            let open = level.rms_db >= -f32::from(n);
            parts.push(
                text(format!(
                    "Voice-activity threshold -{n} dB: your voice is currently {} it.",
                    if open { "above" } else { "below" }
                ))
                .into(),
            );
        }
    }
    parts.push(text(echo_text(state.echo, in_call)).into());
    if let Some(bitrate) = state.send_bitrate.filter(|_| in_call) {
        parts.push(
            text(format!(
                "Sending your voice at {} kbps (the channel's bitrate).",
                bitrate / 1000
            ))
            .into(),
        );
    }
    let can_start = !in_call && !state.running();
    parts.push(
        row![
            button("Check level")
                .on_press_maybe(can_start.then_some(wrap(Event::Start(MicTestMode::Level)))),
            button("Record and play back").on_press_maybe(
                can_start.then_some(wrap(Event::Start(MicTestMode::RecordAndPlay)))
            ),
            button("Stop").on_press_maybe(state.running().then_some(wrap(Event::Stop))),
        ]
        .spacing(12)
        .into(),
    );
    parts.push(text(status_text(state, in_call)).into());
    parts.push(
        text("These checks never reach a call: a recording stays in memory (5 seconds, about 20 KB) and is played only to your selected output.")
            .into(),
    );
    column(parts).spacing(12).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(frames: u64, rms: i16, peak: i16) -> InputDiagnostics {
        InputDiagnostics {
            frames,
            rms_tenths_db: rms,
            peak_tenths_db: peak,
            ..InputDiagnostics::default()
        }
    }

    #[test]
    fn the_meter_follows_the_call_microphone_and_holds_the_peak() {
        let mut state = State::default();
        state.tick(Some(call(5, -300, -120)));
        let level = state.level.expect("a level");
        assert!(level.from_call);
        assert_eq!(level.rms_db, -30.0);
        assert_eq!(state.peak_hold, -12.0);
        // The marker falls 2 dB per tick while the level stays lower.
        state.tick(Some(call(6, -400, -400)));
        assert_eq!(state.peak_hold, -14.0);
        // A muted microphone delivers no frames: no level is shown, not a stale one.
        state.tick(Some(call(0, -1000, -1000)));
        assert!(state.level.is_none());
        // No call and no check: nothing is measured, nothing is shown.
        state.tick(None);
        assert!(state.level.is_none() && state.echo.is_none());
    }

    #[test]
    fn closing_stops_everything_and_invalidates_a_pending_start() {
        let mut state = State {
            starting: true,
            serial: 4,
            ..State::default()
        };
        state.close();
        assert!(!state.running());
        assert_eq!(state.serial, 5);
        let context = Context {
            input: DeviceChoice::SystemDefault,
            output: DeviceChoice::SystemDefault,
            noise_suppression: NoiseSuppression::Light,
            gain: InputGain::default(),
            in_call: false,
        };
        // The late answer of the start that was pending is ignored.
        let _ = state.update(Event::Started(4, Err(MicTestFailure::Silent)), &context);
        assert!(state.notice.is_none());
    }

    #[test]
    fn checks_do_not_start_during_a_call_and_failures_are_explained() {
        let mut state = State::default();
        let mut context = Context {
            input: DeviceChoice::SystemDefault,
            output: DeviceChoice::SystemDefault,
            noise_suppression: NoiseSuppression::Light,
            gain: InputGain::default(),
            in_call: true,
        };
        let _ = state.update(Event::Start(MicTestMode::Level), &context);
        assert!(
            !state.running(),
            "no second microphone stream beside a call"
        );
        assert!(status_text(&state, true).contains("Leave the call"));
        context.in_call = false;
        state.starting = true;
        state.serial = 1;
        let failure = MicTestFailure::Input(fastcord_audio::DeviceErrorKind::PermissionDenied);
        let _ = state.update(Event::Started(1, Err(failure)), &context);
        let text = status_text(&state, false);
        assert!(text.contains("privacy"), "{text}");
        assert!(!state.running());
    }

    #[test]
    fn echo_text_distinguishes_learning_adapted_and_absent() {
        assert!(echo_text(None, false).contains("during a call"));
        assert!(echo_text(None, true).contains("waiting"));
        let learning = EchoDiagnostics::default();
        assert!(echo_text(Some(learning), true).contains("no echo path found"));
        let adapted = EchoDiagnostics {
            reduction_tenths_db: 255,
            delay_ms: 62,
            converged: true,
        };
        let text = echo_text(Some(adapted), true);
        assert!(text.contains("62 ms") && text.contains("26 dB") && text.contains("adapted"));
    }
}
