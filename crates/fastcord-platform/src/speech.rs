//! Native local speech synthesis for read-aloud and TTS messages (issue #104).
//! Nothing leaves the machine: Windows uses the on-device WinRT
//! `SpeechSynthesizer` and plays its stream through a `MediaPlayer`; Linux
//! speaks through the speech-dispatcher daemon (`speech/linux.rs`, SSIP over
//! its Unix socket); macOS through the system `say` tool (`speech/macos.rs`).
//! All speech runs on one dedicated thread fed by a bounded queue; no method
//! here blocks on synthesis, so none stalls the UI thread (`voices` and `start`
//! may block briefly and belong on a worker).

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
use std::sync::mpsc;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as native;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as native;

use parking_lot::Mutex;

/// Utterances waiting behind the one being spoken.
pub const MAX_QUEUE: usize = 8;
/// Text of one utterance, in bytes; longer text is cut at a character boundary.
pub const MAX_TEXT_BYTES: usize = 2048;
/// Speaking rate as a percent of the voice's normal rate.
pub const MIN_RATE: u16 = 50;
pub const MAX_RATE: u16 = 300;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechError {
    Unsupported,
    Failed,
    /// The queue holds [`MAX_QUEUE`] utterances already.
    Full,
}

impl fmt::Display for SpeechError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "Speech synthesis is not supported on this platform.",
            Self::Failed => "The operating system's speech engine is not available.",
            Self::Full => "Too many messages are waiting to be read aloud.",
        })
    }
}

impl std::error::Error for SpeechError {}

/// An installed synthesis voice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Voice {
    /// Stable identifier, what a saved choice refers to.
    pub id: String,
    pub name: String,
    pub language: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Utterance {
    pub text: String,
    /// `Voice::id`; the system default when absent or no longer installed.
    pub voice: Option<String>,
    /// Percent, clamped to `MIN_RATE..=MAX_RATE`.
    pub rate: u16,
}

/// The bounded waiting line. Pure, so its limits are tested without an engine.
#[derive(Default)]
struct Queue(VecDeque<Utterance>);

impl Queue {
    fn push(&mut self, mut utterance: Utterance) -> Result<(), SpeechError> {
        if self.0.len() >= MAX_QUEUE {
            return Err(SpeechError::Full);
        }
        if utterance.text.len() > MAX_TEXT_BYTES {
            let mut end = MAX_TEXT_BYTES;
            while !utterance.text.is_char_boundary(end) {
                end -= 1;
            }
            utterance.text.truncate(end);
        }
        utterance.rate = utterance.rate.clamp(MIN_RATE, MAX_RATE);
        self.0.push_back(utterance);
        Ok(())
    }
}

#[derive(Default)]
struct Shared {
    queue: Queue,
    /// An utterance is being synthesized or played.
    busy: bool,
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
enum Cmd {
    /// The queue changed.
    Wake,
    /// Utterance `.0` (a generation, or speech-dispatcher's message ID)
    /// finished or failed.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Ended(u64),
    /// The connection to the speech service closed.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Lost,
    /// Stop now; the queue was already cleared.
    Stop,
    Shutdown,
}

/// The next utterance to start, marking the speaker busy; `None` while one
/// is already busy or nothing waits.
#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn next_utterance(shared: &Mutex<Shared>) -> Option<Utterance> {
    let mut shared = shared.lock();
    if shared.busy {
        return None;
    }
    let next = shared.queue.0.pop_front();
    shared.busy = next.is_some();
    next
}

/// Handle to the speech thread. Dropping it stops speech and the thread.
pub struct Speaker {
    shared: Arc<Mutex<Shared>>,
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    tx: mpsc::Sender<Cmd>,
}

impl Speaker {
    /// Starts the speech thread.
    pub fn start() -> Result<Self, SpeechError> {
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        {
            let shared = Arc::new(Mutex::new(Shared::default()));
            let (tx, rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::channel();
            let thread = (Arc::clone(&shared), tx.clone());
            std::thread::Builder::new()
                .name("fastcord-speech".to_owned())
                .spawn(move || native::run(thread.0, thread.1, rx, ready_tx))
                .map_err(|_| SpeechError::Failed)?;
            ready_rx.recv().map_err(|_| SpeechError::Failed)??;
            Ok(Self { shared, tx })
        }
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        Err(SpeechError::Unsupported)
    }

    /// Queues one utterance behind what is already waiting.
    pub fn speak(&self, utterance: Utterance) -> Result<(), SpeechError> {
        self.shared.lock().queue.push(utterance)?;
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        let _ = self.tx.send(Cmd::Wake);
        Ok(())
    }

    /// Clears the queue and silences the current utterance.
    pub fn stop(&self) {
        {
            let mut shared = self.shared.lock();
            shared.queue.0.clear();
            shared.busy = false;
        }
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        let _ = self.tx.send(Cmd::Stop);
    }

    pub fn is_speaking(&self) -> bool {
        let shared = self.shared.lock();
        shared.busy || !shared.queue.0.is_empty()
    }
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
impl Drop for Speaker {
    fn drop(&mut self) {
        self.stop();
        let _ = self.tx.send(Cmd::Shutdown);
    }
}

/// Installed voices, for the settings choice. May block briefly.
pub fn voices() -> Result<Vec<Voice>, SpeechError> {
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    return native::voices();
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    Err(SpeechError::Unsupported)
}

#[cfg(windows)]
mod native {
    use super::*;
    use windows::Foundation::TypedEventHandler;
    use windows::Media::Core::MediaSource;
    use windows::Media::Playback::{IMediaPlaybackSource, MediaPlayer, MediaPlayerFailedEventArgs};
    use windows::Media::SpeechSynthesis::{SpeechSynthesizer, VoiceInformation};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::{HSTRING, IInspectable, Interface};

    fn init() {
        // An already-initialized thread reports a harmless error.
        // SAFETY: plain WinRT apartment initialization of the calling thread.
        #[allow(unsafe_code)]
        let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    }

    pub fn voices() -> Result<Vec<Voice>, SpeechError> {
        init();
        let all = SpeechSynthesizer::AllVoices().map_err(|_| SpeechError::Failed)?;
        Ok(all
            .into_iter()
            .filter_map(|voice| {
                Some(Voice {
                    id: voice.Id().ok()?.to_string(),
                    name: voice.DisplayName().ok()?.to_string(),
                    language: voice.Language().ok()?.to_string(),
                })
            })
            .collect())
    }

    fn find_voice(id: &str) -> Option<VoiceInformation> {
        SpeechSynthesizer::AllVoices()
            .ok()?
            .into_iter()
            .find(|voice| voice.Id().is_ok_and(|v| v == id))
    }

    /// Synthesizes `utterance` and starts playing it; the player reports its
    /// end as `Ended(generation)`.
    fn begin(
        synth: &SpeechSynthesizer,
        utterance: &Utterance,
        generation: u64,
        tx: &mpsc::Sender<Cmd>,
    ) -> windows::core::Result<MediaPlayer> {
        if let Some(voice) = utterance.voice.as_deref().and_then(find_voice) {
            synth.SetVoice(&voice)?;
        }
        synth
            .Options()?
            .SetSpeakingRate(f64::from(utterance.rate) / 100.0)?;
        let stream = synth
            .SynthesizeTextToStreamAsync(&HSTRING::from(utterance.text.as_str()))?
            .join()?;
        let source = MediaSource::CreateFromStream(&stream, &stream.ContentType()?)?;
        let player = MediaPlayer::new()?;
        let ended = tx.clone();
        player.MediaEnded(&TypedEventHandler::<MediaPlayer, IInspectable>::new(
            move |_, _| {
                let _ = ended.send(Cmd::Ended(generation));
                Ok(())
            },
        ))?;
        let failed = tx.clone();
        player.MediaFailed(
            &TypedEventHandler::<MediaPlayer, MediaPlayerFailedEventArgs>::new(move |_, _| {
                let _ = failed.send(Cmd::Ended(generation));
                Ok(())
            }),
        )?;
        player.SetSource(&source.cast::<IMediaPlaybackSource>()?)?;
        player.Play()?;
        Ok(player)
    }

    pub fn run(
        shared: Arc<Mutex<Shared>>,
        tx: mpsc::Sender<Cmd>,
        rx: mpsc::Receiver<Cmd>,
        ready: mpsc::Sender<Result<(), SpeechError>>,
    ) {
        init();
        let Ok(synth) = SpeechSynthesizer::new() else {
            let _ = ready.send(Err(SpeechError::Failed));
            return;
        };
        let _ = ready.send(Ok(()));
        let mut generation = 0u64;
        let mut player: Option<MediaPlayer> = None;
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Shutdown => break,
                Cmd::Stop => {
                    generation += 1;
                    if let Some(player) = player.take() {
                        let _ = player.Pause();
                    }
                }
                Cmd::Ended(ended) if ended == generation => {
                    player = None;
                    shared.lock().busy = false;
                }
                Cmd::Ended(_) => continue,
                Cmd::Wake | Cmd::Lost => {}
            }
            // Start the next utterance when idle.
            if player.is_some() {
                continue;
            }
            while let Some(utterance) = next_utterance(&shared) {
                generation += 1;
                match begin(&synth, &utterance, generation, &tx) {
                    // A Stop that raced with synthesis cleared `busy`; honor it.
                    Ok(started) if shared.lock().busy => {
                        player = Some(started);
                        break;
                    }
                    Ok(started) => {
                        let _ = started.Pause();
                        break;
                    }
                    // This utterance failed; try the next one.
                    Err(_) => shared.lock().busy = false,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn say(text: &str) -> Utterance {
        Utterance {
            text: text.to_owned(),
            voice: None,
            rate: 100,
        }
    }

    #[test]
    fn queue_is_bounded_and_clips_text() {
        let mut queue = Queue::default();
        for _ in 0..MAX_QUEUE {
            queue.push(say("hi")).unwrap();
        }
        assert_eq!(queue.push(say("more")), Err(SpeechError::Full));
        assert_eq!(queue.0.len(), MAX_QUEUE);

        let mut queue = Queue::default();
        // 'é' is two bytes: the cut must not split it.
        queue.push(say(&"é".repeat(MAX_TEXT_BYTES))).unwrap();
        let text = &queue.0[0].text;
        assert!(text.len() <= MAX_TEXT_BYTES && text.chars().all(|c| c == 'é'));
        queue
            .push(Utterance {
                rate: 9999,
                ..say("x")
            })
            .unwrap();
        assert_eq!(queue.0[1].rate, MAX_RATE);
    }

    #[test]
    fn stop_clears_the_queue() {
        let shared = Arc::new(Mutex::new(Shared::default()));
        #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
        let speaker = Speaker {
            shared: Arc::clone(&shared),
            tx: mpsc::channel().0,
        };
        #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
        let speaker = Speaker {
            shared: Arc::clone(&shared),
        };
        speaker.speak(say("a")).unwrap();
        speaker.speak(say("b")).unwrap();
        assert!(speaker.is_speaking());
        speaker.stop();
        assert!(!speaker.is_speaking() && shared.lock().queue.0.is_empty());
    }

    /// Manual: speaks through the real engine. Run with `--ignored` on a box
    /// with sound and at least one installed voice (on Linux, speech-dispatcher
    /// with a synthesizer such as espeak-ng).
    #[test]
    #[ignore = "needs an audio device and an installed voice"]
    fn manual_native_synthesizer_speaks() {
        let voices = voices().expect("voices");
        assert!(!voices.is_empty());
        let speaker = Speaker::start().expect("speaker");
        speaker.speak(say("fastcord speech check")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(500));
        assert!(speaker.is_speaking());
        while speaker.is_speaking() {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}
