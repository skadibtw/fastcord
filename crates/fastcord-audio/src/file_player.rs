//! Local Ogg Opus file playback through the audio engine's remote-speaker path.
//!
//! A player is one output-only [`AudioEngine`] plus one control thread. The thread demuxes
//! a [`PacketSource`] ([`OggOpus`], or a container's Opus track) and hands 20 ms packets to the engine paced by the wall clock, never more than
//! [`LEAD`] ahead, so memory is a handful of packets whatever the file length and the engine's
//! own jitter buffer and decoder do the rest. Audio callbacks are untouched: they only read
//! the engine's rings. Dropping the [`FilePlayer`] stops the thread, which drops the engine.
//!
//! Pause, seek and resume restart the packet stream with a sequence jump past the speaker's
//! re-anchor threshold, so late or queued packets of the old position are discarded instead of
//! mixed in. Playback speed is not offered: Opus cannot be time-stretched without decoding,
//! which the engine does inside its own pipeline.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::error::TrySendError;

use crate::codec::EncodedPacket;
use crate::device::{DeviceChoice, Direction};
use crate::ogg::{OggError, OggOpus};
use crate::pipeline::RemoteFrame;
use crate::{AudioEngine, EngineConfig, EngineControl, EngineEvent, NoiseSuppression};

const FRAME: Duration = Duration::from_millis(20);
const FRAME_MS: u64 = 20;
/// How far ahead of the clock packets are queued (five packets).
const LEAD: Duration = Duration::from_millis(100);
/// Estimated time between handing a packet over and hearing it (jitter target plus ring).
const LATENCY_MS: u64 = 120;
/// Added to the sequence on every restart; larger than the speaker's re-anchor distance.
const SEQUENCE_JUMP: u64 = 64;
/// A paused or finished player still looks for a lost device this often.
const IDLE_POLL: Duration = Duration::from_millis(500);
/// How long a paused or ended player sleeps between commands.
const PARKED: Duration = Duration::from_secs(3600);
const SSRC: u32 = 0x4650_4c59;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerFailure {
    /// The output device was lost or could not play.
    Device,
    /// The file is damaged.
    Damaged,
    /// The file is not Ogg Opus this player plays.
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerState {
    Playing,
    Paused,
    Ended,
    Failed(PlayerFailure),
}

impl PlayerState {
    fn code(self) -> u8 {
        match self {
            Self::Playing => 0,
            Self::Paused => 1,
            Self::Ended => 2,
            Self::Failed(PlayerFailure::Device) => 3,
            Self::Failed(PlayerFailure::Damaged) => 4,
            Self::Failed(PlayerFailure::Unsupported) => 5,
        }
    }

    fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Playing,
            1 => Self::Paused,
            2 => Self::Ended,
            3 => Self::Failed(PlayerFailure::Device),
            4 => Self::Failed(PlayerFailure::Damaged),
            _ => Self::Failed(PlayerFailure::Unsupported),
        }
    }
}

/// Where a player gets its 20 ms Opus packets from.
pub trait PacketSource: Send + 'static {
    /// Length of the audio, when known.
    fn duration_ms(&self) -> Option<u64>;
    /// The next packet, or `None` at the end.
    fn next_packet(&mut self) -> Result<Option<EncodedPacket>, PlayerFailure>;
    /// Repositions so that the next packet starts at `ms` (to the packet).
    fn seek_ms(&mut self, ms: u64) -> Result<(), PlayerFailure>;
}

impl<R: Read + Seek + Send + 'static> PacketSource for OggOpus<R> {
    fn duration_ms(&self) -> Option<u64> {
        OggOpus::duration_ms(self)
    }

    fn next_packet(&mut self) -> Result<Option<EncodedPacket>, PlayerFailure> {
        OggOpus::next_packet(self).map_err(Into::into)
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), PlayerFailure> {
        OggOpus::seek_ms(self, ms).map_err(Into::into)
    }
}

/// Silent packets for a given length: the clock of a video without playable audio. Paired
/// with no output device, the player then only paces, pauses and seeks.
pub struct Silence {
    duration_ms: u64,
    position_ms: u64,
}

impl Silence {
    pub fn new(duration_ms: u64) -> Self {
        Self {
            duration_ms,
            position_ms: 0,
        }
    }
}

impl PacketSource for Silence {
    fn duration_ms(&self) -> Option<u64> {
        Some(self.duration_ms)
    }

    fn next_packet(&mut self) -> Result<Option<EncodedPacket>, PlayerFailure> {
        if self.position_ms >= self.duration_ms {
            return Ok(None);
        }
        self.position_ms += FRAME_MS;
        // A 20 ms CELT table-of-contents byte alone: a lost frame, which decodes to silence.
        EncodedPacket::from_slice(&[0xF8])
            .map(Some)
            .map_err(|_| PlayerFailure::Unsupported)
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), PlayerFailure> {
        self.position_ms = ms.min(self.duration_ms);
        Ok(())
    }
}

impl From<OggError> for PlayerFailure {
    fn from(error: OggError) -> Self {
        match error {
            OggError::Io | OggError::Malformed => Self::Damaged,
            OggError::Unsupported => Self::Unsupported,
        }
    }
}

/// What a player is doing right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayerSnapshot {
    pub state: PlayerState,
    pub position_ms: u64,
    pub duration_ms: Option<u64>,
}

/// Why a player could not start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayerError {
    /// The file is unreadable, damaged or not playable Ogg Opus.
    File(PlayerFailure),
    /// The audio output could not be opened.
    Device,
}

#[derive(Default)]
struct Shared {
    /// Counts every restart of the stream (seek, resume, replay); see [`PlayerClock`].
    epoch: AtomicU64,
    state: AtomicU8,
    position_ms: AtomicU64,
    duration_ms: AtomicU64,
}

impl Shared {
    fn snapshot(&self) -> PlayerSnapshot {
        let duration = self.duration_ms.load(Ordering::Acquire);
        PlayerSnapshot {
            state: PlayerState::from_code(self.state.load(Ordering::Acquire)),
            position_ms: self.position_ms.load(Ordering::Acquire),
            duration_ms: (duration != u64::MAX).then_some(duration),
        }
    }

    fn set_state(&self, state: PlayerState) {
        self.state.store(state.code(), Ordering::Release);
    }
}

enum Command {
    Play,
    Pause,
    Seek(u64),
    Volume(f32),
    Muted(bool),
}

/// Where the control thread puts packets; the real one is the audio engine.
trait Sink {
    fn offer(&mut self, frame: &RemoteFrame) -> Offer;
    /// Applies a linear gain; false means "queue full, ask again".
    fn gain(&mut self, gain: f32) -> bool;
    /// Whether the output device failed.
    fn failed(&mut self) -> bool;
    /// Estimated time between handing a packet over and hearing it.
    fn latency_ms(&self) -> u64;
    /// Paused or ended: release the output (no stream, no engine thread).
    fn suspend(&mut self) {}
    /// About to play again: reopen the output; false if it cannot be opened.
    fn resume(&mut self) -> bool {
        true
    }
}

/// No output at all: packets are paced and dropped.
struct NullSink;

impl Sink for NullSink {
    fn offer(&mut self, _: &RemoteFrame) -> Offer {
        Offer::Sent
    }

    fn gain(&mut self, _: f32) -> bool {
        true
    }

    fn failed(&mut self) -> bool {
        false
    }

    fn latency_ms(&self) -> u64 {
        0
    }
}

enum Offer {
    Sent,
    Full,
    Closed,
}

/// An output-only engine that exists only while the player plays: a paused or
/// finished attachment holds no audio stream and no engine thread (SPEC 2.3).
struct EngineSink {
    output: DeviceChoice,
    open: Option<OpenEngine>,
}

struct OpenEngine {
    // Held so the streams live exactly as long as the sink.
    _engine: AudioEngine,
    remote: tokio::sync::mpsc::Sender<RemoteFrame>,
    controls: tokio::sync::mpsc::Sender<EngineControl>,
    events: tokio::sync::mpsc::Receiver<EngineEvent>,
}

impl OpenEngine {
    /// Blocks while the device opens.
    fn start(output: &DeviceChoice) -> Result<Self, PlayerError> {
        let (engine, channels) = AudioEngine::start(EngineConfig {
            input: None,
            output: Some(output.clone()),
            noise_suppression: NoiseSuppression::Off,
            echo_cancellation: false,
            capture_enabled: false,
            ..EngineConfig::default()
        })
        .map_err(|_| PlayerError::Device)?;
        Ok(Self {
            _engine: engine,
            remote: channels.remote_audio.ok_or(PlayerError::Device)?,
            controls: channels.controls,
            events: channels.events,
        })
    }
}

impl Sink for EngineSink {
    fn offer(&mut self, frame: &RemoteFrame) -> Offer {
        let Some(open) = &self.open else {
            return Offer::Closed;
        };
        // The copy is one 20 ms packet; keeping the original lets a full queue be retried.
        match open.remote.try_send(frame.clone()) {
            Ok(()) => Offer::Sent,
            Err(TrySendError::Full(_)) => Offer::Full,
            Err(TrySendError::Closed(_)) => Offer::Closed,
        }
    }

    fn gain(&mut self, gain: f32) -> bool {
        // Suspended: the next engine gets the gain when it opens.
        self.open.as_ref().is_none_or(|open| {
            open.controls
                .try_send(EngineControl::RemoteGain { ssrc: SSRC, gain })
                .is_ok()
        })
    }

    fn latency_ms(&self) -> u64 {
        LATENCY_MS
    }

    fn suspend(&mut self) {
        self.open = None;
    }

    fn resume(&mut self) -> bool {
        if self.open.is_none() {
            self.open = OpenEngine::start(&self.output).ok();
        }
        self.open.is_some()
    }

    fn failed(&mut self) -> bool {
        let Some(open) = &mut self.open else {
            return false;
        };
        while let Ok(event) = open.events.try_recv() {
            if matches!(
                event,
                EngineEvent::StreamFailed {
                    direction: Direction::Output,
                    ..
                }
            ) {
                return true;
            }
        }
        false
    }
}

/// The audible position of a player, readable from any thread (the master clock of a video).
#[derive(Clone)]
pub struct PlayerClock(Arc<Shared>);

impl PlayerClock {
    pub fn snapshot(&self) -> PlayerSnapshot {
        self.0.snapshot()
    }

    /// Increases whenever the player restarts its stream (seek, resume, replay), after the new
    /// position is published. A follower that asked for a seek waits for it to grow.
    pub fn epoch(&self) -> u64 {
        self.0.epoch.load(Ordering::Acquire)
    }
}

/// One playing, paused or finished audio file. Cheap to poll from the UI thread.
pub struct FilePlayer {
    shared: Arc<Shared>,
    commands: mpsc::Sender<Command>,
}

impl std::fmt::Debug for FilePlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilePlayer")
            .field("snapshot", &self.snapshot())
            .finish()
    }
}

impl FilePlayer {
    /// Validates `path` and opens `output`, then starts playing from the beginning. Blocks
    /// while the device opens: call it off the UI thread.
    pub fn start(path: &Path, output: DeviceChoice) -> Result<Self, PlayerError> {
        let file = File::open(path).map_err(|_| PlayerError::File(PlayerFailure::Damaged))?;
        let reader =
            OggOpus::open(BufReader::new(file)).map_err(|e| PlayerError::File(e.into()))?;
        Self::start_source(reader, Some(output))
    }

    /// Starts playing `source` from the beginning. With `output` it opens that device; with
    /// `None` nothing is heard and the player is only a clock (see [`Silence`]). Blocks while
    /// a device opens: call it off the UI thread.
    pub fn start_source(
        source: impl PacketSource,
        output: Option<DeviceChoice>,
    ) -> Result<Self, PlayerError> {
        let Some(output) = output else {
            return Ok(Self::spawn(source, NullSink));
        };
        let open = OpenEngine::start(&output)?;
        Ok(Self::spawn(
            source,
            EngineSink {
                output,
                open: Some(open),
            },
        ))
    }

    fn spawn(reader: impl PacketSource, sink: impl Sink + Send + 'static) -> Self {
        let shared = Arc::new(Shared::default());
        shared
            .duration_ms
            .store(reader.duration_ms().unwrap_or(u64::MAX), Ordering::Release);
        let (commands, receiver) = mpsc::channel();
        let thread_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("fastcord-file-player".to_owned())
            .spawn(move || run(reader, sink, &receiver, &thread_shared));
        if spawned.is_err() {
            shared.set_state(PlayerState::Failed(PlayerFailure::Device));
        }
        Self { shared, commands }
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        self.shared.snapshot()
    }

    /// A handle that reads this player's position from another thread.
    pub fn clock(&self) -> PlayerClock {
        PlayerClock(Arc::clone(&self.shared))
    }

    /// Starts or resumes; from the end it starts again from the beginning.
    pub fn play(&self) {
        let _ = self.commands.send(Command::Play);
    }

    pub fn pause(&self) {
        let _ = self.commands.send(Command::Pause);
    }

    /// Moves to `ms` and keeps the current play/pause state.
    pub fn seek(&self, ms: u64) {
        let _ = self.commands.send(Command::Seek(ms));
    }

    /// Linear volume, clamped to 0–1.
    pub fn set_volume(&self, volume: f32) {
        let _ = self.commands.send(Command::Volume(volume.clamp(0.0, 1.0)));
    }

    pub fn set_muted(&self, muted: bool) {
        let _ = self.commands.send(Command::Muted(muted));
    }
}

/// The control thread. Returns when the player is dropped or has failed.
fn run(
    mut reader: impl PacketSource,
    mut sink: impl Sink,
    commands: &mpsc::Receiver<Command>,
    shared: &Shared,
) {
    let duration = reader.duration_ms();
    let mut playing = true;
    let mut ended = false;
    // Position of the first packet since the last (re)start and packets handed over since.
    let mut base_ms = 0u64;
    let mut sent = 0u64;
    let mut resume_ms = 0u64;
    let mut next_due = Instant::now();
    let mut drain_until: Option<Instant> = None;
    let mut pending: Option<RemoteFrame> = None;
    let mut sequence = 0u64;
    let mut timestamp = 0u32;
    let (mut volume, mut muted, mut applied) = (1.0f32, false, None::<f32>);
    shared.set_state(PlayerState::Playing);
    let latency = sink.latency_ms();
    let audible = |base: u64, sent: u64| {
        let heard = (base + sent * FRAME_MS).saturating_sub(latency);
        duration.map_or(heard, |end| heard.min(end))
    };
    let mut wait = Duration::ZERO;
    loop {
        let first = match commands.recv_timeout(wait) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // Everything that changes the stream position funnels through `restart`.
        let mut restart: Option<(u64, bool)> = None;
        for command in first.into_iter().chain(commands.try_iter()) {
            match command {
                Command::Volume(value) => volume = value,
                Command::Muted(value) => muted = value,
                Command::Pause if playing => {
                    resume_ms = audible(base_ms, sent);
                    playing = false;
                    sink.suspend();
                    shared.position_ms.store(resume_ms, Ordering::Release);
                    shared.set_state(PlayerState::Paused);
                }
                Command::Play if !playing => {
                    restart = Some((if ended { 0 } else { resume_ms }, true));
                }
                Command::Seek(ms) => {
                    restart = Some((duration.map_or(ms, |end| ms.min(end)), playing));
                }
                Command::Pause | Command::Play => {}
            }
        }
        let wanted = if muted { 0.0 } else { volume };
        if applied != Some(wanted) && sink.gain(wanted) {
            applied = Some(wanted);
        }
        if sink.failed() {
            return shared.set_state(PlayerState::Failed(PlayerFailure::Device));
        }
        if let Some((at, play)) = restart {
            if let Err(failure) = reader.seek_ms(at) {
                return shared.set_state(PlayerState::Failed(failure));
            }
            if play && !playing {
                if !sink.resume() {
                    return shared.set_state(PlayerState::Failed(PlayerFailure::Device));
                }
                // A reopened engine starts at unity gain.
                applied = None;
                if sink.gain(wanted) {
                    applied = Some(wanted);
                }
            }
            pending = None;
            drain_until = None;
            ended = false;
            base_ms = at;
            sent = 0;
            resume_ms = at;
            sequence += SEQUENCE_JUMP;
            next_due = Instant::now();
            playing = play;
            shared.position_ms.store(at, Ordering::Release);
            shared.set_state(if play {
                PlayerState::Playing
            } else {
                PlayerState::Paused
            });
            shared.epoch.fetch_add(1, Ordering::AcqRel);
        }
        let now = Instant::now();
        // Paused or ended, the output is closed: wait for a command.
        wait = if playing { IDLE_POLL } else { PARKED };
        if now > next_due + LEAD {
            // The thread was starved (suspend, debugger): resume in real time, not in a burst.
            next_due = now;
        }
        if !playing {
            continue;
        }
        if let Some(end) = drain_until {
            if now >= end {
                playing = false;
                ended = true;
                drain_until = None;
                sink.suspend();
                resume_ms = duration.unwrap_or(base_ms + sent * FRAME_MS);
                shared.position_ms.store(resume_ms, Ordering::Release);
                shared.set_state(PlayerState::Ended);
            } else {
                wait = end - now;
            }
        } else if now + LEAD < next_due {
            wait = next_due - LEAD - now;
        } else {
            let frame = match pending.take() {
                Some(frame) => frame,
                None => match reader.next_packet() {
                    Ok(Some(packet)) => RemoteFrame {
                        ssrc: SSRC,
                        sequence,
                        timestamp,
                        packet,
                        read_at: now,
                    },
                    Ok(None) => {
                        // Let what was queued be heard before reporting the end.
                        drain_until = Some(next_due.max(now) + Duration::from_millis(latency));
                        wait = Duration::ZERO;
                        continue;
                    }
                    Err(failure) => return shared.set_state(PlayerState::Failed(failure)),
                },
            };
            match sink.offer(&frame) {
                Offer::Sent => {
                    sequence += 1;
                    timestamp = timestamp.wrapping_add(FRAME_MS as u32 * 48);
                    sent += 1;
                    next_due += FRAME;
                    shared
                        .position_ms
                        .store(audible(base_ms, sent), Ordering::Release);
                    wait = Duration::ZERO;
                }
                Offer::Full => {
                    pending = Some(frame);
                    wait = Duration::from_millis(5);
                }
                Offer::Closed => {
                    return shared.set_state(PlayerState::Failed(PlayerFailure::Device));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg::testutil::{packet_id, sample};
    use std::io::Cursor;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    struct Log {
        frames: Vec<(Instant, u64, u16)>,
        gains: Vec<f32>,
        /// Offers refused because the sink was full.
        refused: usize,
        /// Whether the output is open (between `resume` and `suspend`), and
        /// offers made while it was closed.
        suspended: bool,
        offered_while_suspended: usize,
        resumes: usize,
    }

    struct Recorder {
        log: Arc<Mutex<Log>>,
        fail: Arc<AtomicBool>,
        full: Arc<AtomicBool>,
    }

    impl Sink for Recorder {
        fn offer(&mut self, frame: &RemoteFrame) -> Offer {
            if self.full.load(Ordering::Acquire) {
                self.log.lock().unwrap().refused += 1;
                return Offer::Full;
            }
            if self.log.lock().unwrap().suspended {
                self.log.lock().unwrap().offered_while_suspended += 1;
            }
            let id = packet_id(frame.packet.as_bytes());
            self.log
                .lock()
                .unwrap()
                .frames
                .push((Instant::now(), frame.sequence, id));
            Offer::Sent
        }

        fn gain(&mut self, gain: f32) -> bool {
            self.log.lock().unwrap().gains.push(gain);
            true
        }

        fn failed(&mut self) -> bool {
            self.fail.load(Ordering::Acquire)
        }

        fn latency_ms(&self) -> u64 {
            LATENCY_MS
        }

        fn suspend(&mut self) {
            self.log.lock().unwrap().suspended = true;
        }

        fn resume(&mut self) -> bool {
            let mut log = self.log.lock().unwrap();
            log.suspended = false;
            log.resumes += 1;
            true
        }
    }

    struct Rig {
        player: FilePlayer,
        log: Arc<Mutex<Log>>,
        fail: Arc<AtomicBool>,
        full: Arc<AtomicBool>,
    }

    fn rig(bytes: Vec<u8>) -> Rig {
        let log = Arc::new(Mutex::new(Log::default()));
        let (fail, full) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
        );
        let reader = OggOpus::open(Cursor::new(bytes)).unwrap();
        let player = FilePlayer::spawn(
            reader,
            Recorder {
                log: Arc::clone(&log),
                fail: Arc::clone(&fail),
                full: Arc::clone(&full),
            },
        );
        Rig {
            player,
            log,
            fail,
            full,
        }
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    impl Rig {
        fn state(&self) -> PlayerState {
            self.player.snapshot().state
        }

        fn ids(&self) -> Vec<u16> {
            self.log
                .lock()
                .unwrap()
                .frames
                .iter()
                .map(|f| f.2)
                .collect()
        }
    }

    #[test]
    fn plays_every_packet_in_order_paced_by_the_clock_then_ends() {
        let started = Instant::now();
        let rig = rig(sample(25, 9));
        assert_eq!(
            rig.player.snapshot().duration_ms,
            Some((25 * 960 - 312) / 48)
        );
        wait_for(|| rig.state() == PlayerState::Ended);
        let elapsed = started.elapsed();
        assert_eq!(rig.ids(), (0..25).collect::<Vec<_>>());
        let log = rig.log.lock().unwrap();
        // 25 packets = 500 ms; at most 100 ms ahead of the clock, then 120 ms of drain.
        assert!(elapsed >= Duration::from_millis(480), "{elapsed:?}");
        let early = log
            .frames
            .iter()
            .filter(|f| f.0 < started + Duration::from_millis(40))
            .count();
        assert!(early <= 7, "{early} packets sent in the first 40 ms");
        for pair in log.frames.windows(2) {
            assert_eq!(pair[1].1, pair[0].1 + 1);
        }
        drop(log);
        let snapshot = rig.player.snapshot();
        assert_eq!(snapshot.position_ms, snapshot.duration_ms.unwrap());
    }

    #[test]
    fn pause_stops_the_stream_and_resume_replays_what_was_not_heard() {
        let rig = rig(sample(500, 9));
        wait_for(|| rig.ids().len() >= 10);
        rig.player.pause();
        wait_for(|| rig.state() == PlayerState::Paused);
        std::thread::sleep(Duration::from_millis(60));
        let held = rig.ids();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(rig.ids(), held, "nothing is sent while paused");
        let last_seq = rig.log.lock().unwrap().frames.last().unwrap().1;
        rig.player.play();
        wait_for(|| rig.ids().len() > held.len());
        let log = rig.log.lock().unwrap();
        let resumed = log.frames[held.len()];
        // Restarts before the newest packet (the last ~120 ms were not yet heard), after a
        // sequence jump the speaker re-anchors on.
        assert!(usize::from(resumed.2) < held.len());
        assert!(resumed.1 >= last_seq + SEQUENCE_JUMP);
    }

    #[test]
    fn seek_while_playing_and_while_paused_lands_on_the_packet() {
        let rig = rig(sample(500, 9));
        wait_for(|| !rig.ids().is_empty());
        rig.player.seek(4000);
        wait_for(|| rig.ids().iter().any(|&id| id >= 150));
        let ids = rig.ids();
        let first_after = ids.iter().position(|&id| id >= 150).unwrap();
        assert_eq!(ids[first_after], 200);
        rig.player.pause();
        wait_for(|| rig.state() == PlayerState::Paused);
        rig.player.seek(100);
        wait_for(|| rig.player.snapshot().position_ms == 100);
        assert_eq!(rig.state(), PlayerState::Paused);
        let sent = rig.ids().len();
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(rig.ids().len(), sent);
        rig.player.play();
        wait_for(|| rig.ids().len() > sent);
        assert_eq!(rig.ids()[sent], 5);
    }

    #[test]
    fn replay_after_the_end_starts_from_the_beginning() {
        let rig = rig(sample(5, 9));
        wait_for(|| rig.state() == PlayerState::Ended);
        rig.player.play();
        wait_for(|| rig.ids().len() >= 10);
        assert_eq!(rig.ids()[5], 0);
        wait_for(|| rig.state() == PlayerState::Ended);
    }

    #[test]
    fn volume_and_mute_reach_the_sink_without_repeats() {
        let rig = rig(sample(500, 9));
        wait_for(|| !rig.log.lock().unwrap().gains.is_empty());
        rig.player.set_volume(0.5);
        rig.player.set_muted(true);
        rig.player.set_muted(false);
        rig.player.set_volume(9.0);
        wait_for(|| rig.log.lock().unwrap().gains.last() == Some(&1.0));
        let gains = rig.log.lock().unwrap().gains.clone();
        assert_eq!(gains.first(), Some(&1.0));
        assert!(gains.windows(2).all(|pair| pair[0] != pair[1]), "{gains:?}");
    }

    /// A paused or finished player releases its output (no engine, no device
    /// stream) and reopens it only to play again.
    #[test]
    fn paused_and_ended_players_release_the_output() {
        let rig = rig(sample(10, 9));
        wait_for(|| rig.ids().len() >= 2);
        rig.player.pause();
        wait_for(|| rig.state() == PlayerState::Paused);
        assert!(rig.log.lock().unwrap().suspended, "paused holds no output");
        rig.player.play();
        wait_for(|| rig.state() == PlayerState::Ended);
        let log = rig.log.lock().unwrap();
        assert_eq!(log.resumes, 1, "play reopened the output once");
        assert!(log.suspended, "the end releases the output");
        assert_eq!(log.offered_while_suspended, 0);
    }

    #[test]
    fn a_full_queue_is_retried_without_losing_or_reordering_packets() {
        let rig = rig(sample(30, 9));
        rig.full.store(true, Ordering::Release);
        // Packets offered before the flag was seen may have landed; once the
        // sink has refused, the player must retry the same packet and send
        // nothing new, however the threads were scheduled.
        let refused = || rig.log.lock().unwrap().refused;
        wait_for(|| refused() >= 1);
        let landed = rig.ids();
        let seen = refused();
        wait_for(|| refused() >= seen + 3);
        assert_eq!(rig.ids(), landed, "nothing is sent while the sink is full");
        rig.full.store(false, Ordering::Release);
        wait_for(|| rig.state() == PlayerState::Ended);
        assert_eq!(rig.ids(), (0..30).collect::<Vec<_>>());
    }

    #[test]
    fn a_lost_device_fails_the_player_and_stops_sending() {
        let rig = rig(sample(500, 9));
        wait_for(|| !rig.ids().is_empty());
        rig.fail.store(true, Ordering::Release);
        wait_for(|| rig.state() == PlayerState::Failed(PlayerFailure::Device));
        let sent = rig.ids().len();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(rig.ids().len(), sent);
    }

    #[test]
    fn damaged_media_midway_fails_as_damaged() {
        let mut bytes = sample(100, 9);
        let at = bytes.len() * 3 / 4;
        bytes[at] ^= 0xff;
        let rig = rig(bytes);
        wait_for(|| matches!(rig.state(), PlayerState::Failed(_)));
        assert_eq!(rig.state(), PlayerState::Failed(PlayerFailure::Damaged));
    }

    #[test]
    fn dropping_the_player_releases_the_sink() {
        struct Probe {
            _token: Arc<()>,
        }
        impl Sink for Probe {
            fn offer(&mut self, _: &RemoteFrame) -> Offer {
                Offer::Sent
            }
            fn gain(&mut self, _: f32) -> bool {
                true
            }
            fn failed(&mut self) -> bool {
                false
            }
            fn latency_ms(&self) -> u64 {
                0
            }
        }
        let token = Arc::new(());
        let reader = OggOpus::open(Cursor::new(sample(500, 9))).unwrap();
        let player = FilePlayer::spawn(
            reader,
            Probe {
                _token: Arc::clone(&token),
            },
        );
        assert_eq!(Arc::strong_count(&token), 2);
        drop(player);
        wait_for(|| Arc::strong_count(&token) == 1);
    }

    /// Manual hardware check: `cargo test -p fastcord-audio --lib -- --ignored plays_a_tone`.
    /// Plays one second of 440 Hz through the default output device.
    #[test]
    #[ignore = "needs an audio output device"]
    fn plays_a_tone_through_the_default_output_device() {
        use crate::codec::{EncodedPacket, VoiceEncoder};
        use crate::ogg::testutil::{head, stream};
        let mut encoder = VoiceEncoder::new(32_000).unwrap();
        let packets: Vec<Vec<u8>> = (0..50)
            .map(|n| {
                let mut mono = [0f32; crate::codec::FRAME_SAMPLES];
                for (i, sample) in mono.iter_mut().enumerate() {
                    let t = (n * crate::codec::FRAME_SAMPLES + i) as f32 / 48_000.0;
                    *sample = (t * 440.0 * std::f32::consts::TAU).sin() * 0.2;
                }
                let mut out = EncodedPacket::from_slice(&[]).unwrap();
                encoder.encode_mono(&mono, &mut out).unwrap();
                out.as_bytes().to_vec()
            })
            .collect();
        let path = std::env::temp_dir().join("fastcord-tone-test.ogg");
        std::fs::write(&path, stream(&head(1, 0), &packets, 255)).unwrap();
        let player = FilePlayer::start(&path, DeviceChoice::SystemDefault).unwrap();
        wait_for(|| player.snapshot().state == PlayerState::Ended);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_silent_clock_needs_no_device_and_counts_restarts() {
        let player = FilePlayer::start_source(Silence::new(300), None).unwrap();
        let clock = player.clock();
        assert_eq!(player.snapshot().duration_ms, Some(300));
        wait_for(|| clock.snapshot().state == PlayerState::Ended);
        assert_eq!(clock.snapshot().position_ms, 300);
        let before = clock.epoch();
        player.seek(100);
        wait_for(|| clock.epoch() > before);
        // Seeking an ended clock keeps it paused until asked to play, then it runs on.
        player.play();
        wait_for(|| clock.snapshot().state == PlayerState::Ended);
        assert_eq!(clock.snapshot().position_ms, 300);
        assert!(clock.epoch() > before);
    }
}
