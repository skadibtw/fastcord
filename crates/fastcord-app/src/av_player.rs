//! Inline playback of Ogg Opus, MP4 and WebM attachments (issues #74 and #116).
//!
//! One [`MediaPlayer`] owns up to three threads and nothing on the UI thread:
//!
//! * the audio clock, a [`FilePlayer`]. Its packets come from an Ogg Opus file or from the
//!   Opus track of a container ([`ContainerAudio`]); a video without playable audio gets a
//!   [`Silence`] clock with no output device. It paces, pauses, seeks, ends and sets the
//!   volume, and its audible position is the master clock of everything else;
//! * the video worker ([`video_thread`]), which owns the decoder and its own file handle.
//!   It decodes one picture ahead of the clock, drops pictures that are already late and
//!   hands the due one to the latest-frame slot of [`playback::Session`] (the same
//!   lease/notification path as a watched stream). While paused or finished it blocks on
//!   its command channel, so a paused or idle player costs nothing;
//! * a short-lived decoder owner is the video worker itself: codec objects belong to the
//!   thread that opened them.
//!
//! Memory is bounded by construction: the sample index (at most 400 000 entries, 9.6 MB, a
//! few hundred kilobytes for a typical clip), one stored sample and one access unit (each
//! at most 8 MiB), at most two decoded pictures waiting plus the three-buffer, 48 MiB
//! [`FramePool`], and the audio engine's own bounded rings. A/V sync is best effort: the
//! audio position is an estimate (packets are queued up to 100 ms ahead and the output
//! latency is assumed), so video may be off by about 100 ms. A picture that is already due
//! when a newer one arrives is dropped, which is how the video catches up after a seek or
//! a slow decode.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use fastcord_audio::aac::AacTranscoder;
use fastcord_audio::ogg::packet_samples;
use fastcord_audio::{
    DeviceChoice, EncodedPacket, FilePlayer, PacketSource, PlayerClock, PlayerError, PlayerFailure,
    PlayerSnapshot, PlayerState, Silence,
};
use fastcord_video::container::{
    self, AudioCodecId, AudioSkipped, ContainerError, Media, VideoCodecId, VideoTrack,
};
use fastcord_video::frame::{FrameLease, FramePool};
use fastcord_video::{CodecError, DecodedFrame, VideoDecoder};

use crate::playback::{self, Status};

const AAC_UNSUPPORTED: &str = "This AAC audio cannot be decoded here; playing without sound.";
const OTHER_UNSUPPORTED: &str = "This audio format is not supported; playing without sound.";
/// Longest the worker sleeps while waiting for the next picture to fall due.
const MAX_SLEEP: Duration = Duration::from_millis(100);
/// How long a follower waits for the audio thread to act on a command.
const SETTLE: Duration = Duration::from_millis(250);
/// Consecutive undecodable frames before the video gives up (the sound continues).
const MAX_DECODE_FAILURES: u32 = 30;
/// Decoded pictures waiting for their time.
const MAX_PENDING: usize = 2;

/// Why a file cannot start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// The file is unreadable or damaged.
    Damaged,
    /// An Ogg file that is not 20 ms Opus.
    UnsupportedAudio,
    /// A video codec this client cannot play (H.265, VP8, VP9, ...), or neither video nor
    /// Opus audio at all.
    UnsupportedVideo,
    /// A container layout that is not supported (fragmented MP4).
    UnsupportedLayout,
    /// More samples or larger samples than the player accepts.
    TooLarge,
    /// The audio output could not be opened.
    Device,
    /// No H.264 or AV1 decoder could be opened.
    Decoder,
}

impl From<ContainerError> for StartError {
    fn from(error: ContainerError) -> Self {
        match error {
            ContainerError::Io | ContainerError::Malformed => Self::Damaged,
            ContainerError::TooLarge => Self::TooLarge,
            ContainerError::Fragmented => Self::UnsupportedLayout,
            ContainerError::UnsupportedVideo
            | ContainerError::NoPlayableTrack
            | ContainerError::UnknownFormat => Self::UnsupportedVideo,
        }
    }
}

type OpenDecoder =
    Box<dyn FnOnce(VideoCodecId) -> Result<Box<dyn VideoDecoder>, CodecError> + Send>;

fn open_decoder(codec: VideoCodecId) -> Result<Box<dyn VideoDecoder>, CodecError> {
    match codec {
        VideoCodecId::Av1 => Ok(Box::new(fastcord_video::av1::Decoder::open()?)),
        VideoCodecId::H264 => crate::gateway::open_h264(),
    }
}

/// What the video worker is told by the player.
#[derive(Clone, Copy, Debug)]
enum Command {
    /// The audio clock changed state (resume): look at it again.
    Wake,
    /// Restart at `ms`; `epoch` is the clock's restart count the seek will produce.
    Seek { ms: u64, epoch: u64 },
}

struct VideoWorker {
    commands: Sender<Command>,
    session: Arc<playback::Session>,
    size: (u32, u32),
    #[cfg(test)]
    presented: Arc<AtomicU64>,
}

impl Drop for VideoWorker {
    fn drop(&mut self) {
        // The thread notices the closed channel and drops its decoder; nothing waits for it.
        self.session.close();
    }
}

/// One playing, paused or finished attachment. Cheap to poll and control from the UI thread.
pub struct MediaPlayer {
    audio: FilePlayer,
    clock: PlayerClock,
    video: Option<VideoWorker>,
    notice: Option<&'static str>,
}

impl std::fmt::Debug for MediaPlayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaPlayer")
            .field("snapshot", &self.snapshot())
            .field("video", &self.video.is_some())
            .finish()
    }
}

impl MediaPlayer {
    /// Opens `path` (Ogg Opus, MP4 or WebM by content) and starts playing it. Blocks on file
    /// and device I/O: call it off the UI thread.
    pub fn start(path: &Path, output: DeviceChoice) -> Result<Self, StartError> {
        Self::start_with(path, Some(output), Box::new(open_decoder))
    }

    /// `output: None` plays without sound (no device is opened), which tests rely on.
    fn start_with(
        path: &Path,
        output: Option<DeviceChoice>,
        open: OpenDecoder,
    ) -> Result<Self, StartError> {
        let mut file = File::open(path).map_err(|_| StartError::Damaged)?;
        let mut head = [0u8; 8];
        let got = read_head(&mut file, &mut head)?;
        if container::sniff(&head[..got]).is_none() {
            let audio = match output {
                Some(output) => FilePlayer::start(path, output),
                None => return Err(StartError::Device),
            }
            .map_err(|error| match error {
                PlayerError::File(PlayerFailure::Unsupported) => StartError::UnsupportedAudio,
                PlayerError::File(_) => StartError::Damaged,
                PlayerError::Device => StartError::Device,
            })?;
            let clock = audio.clock();
            return Ok(Self {
                audio,
                clock,
                video: None,
                notice: None,
            });
        }
        file.rewind().map_err(|_| StartError::Damaged)?;
        let media = Arc::new(container::open(&mut BufReader::new(file))?);
        let mut notice = media.audio_skipped.map(|skipped| match skipped {
            AudioSkipped::Aac => AAC_UNSUPPORTED,
            AudioSkipped::Other => OTHER_UNSUPPORTED,
        });
        let reader = || {
            File::open(path)
                .map(BufReader::new)
                .map_err(|_| StartError::Damaged)
        };
        // Audio: an Opus track in 20 ms packets, an AAC-LC track transcoded to them, otherwise
        // silence of the media's length.
        let mut audio = None;
        if media.audio.is_some() && output.is_some() {
            match ContainerAudio::open(&media, reader()?) {
                Ok(source) => {
                    audio =
                        Some(FilePlayer::start_source(source, output).map_err(
                            |error| match error {
                                PlayerError::Device => StartError::Device,
                                PlayerError::File(_) => StartError::Damaged,
                            },
                        )?);
                }
                Err(text) => notice = Some(text),
            }
        }
        let audio = match audio {
            Some(audio) => audio,
            None => FilePlayer::start_source(Silence::new(media.duration_ms), None)
                .map_err(|_| StartError::Device)?,
        };
        let clock = audio.clock();
        let video = match media.video {
            Some(_) => Some(start_video(
                Arc::clone(&media),
                reader()?,
                clock.clone(),
                open,
            )?),
            None => None,
        };
        Ok(Self {
            audio,
            clock,
            video,
            notice,
        })
    }

    pub fn snapshot(&self) -> PlayerSnapshot {
        self.audio.snapshot()
    }

    /// A line to show next to the controls (audio that is not played), if any.
    pub fn notice(&self) -> Option<&'static str> {
        self.notice
    }

    /// The latest-frame slot and the picture size, when the file has video.
    pub fn video(&self) -> Option<(Arc<playback::Session>, (u32, u32))> {
        self.video
            .as_ref()
            .map(|video| (Arc::clone(&video.session), video.size))
    }

    /// Whether the video decoder gave up (the sound, if any, continues).
    pub fn video_failed(&self) -> bool {
        self.video
            .as_ref()
            .is_some_and(|video| video.session.failed())
    }

    /// Pictures handed to the presentation slot so far.
    #[cfg(test)]
    fn presented(&self) -> u64 {
        self.video
            .as_ref()
            .map_or(0, |video| video.presented.load(Ordering::Acquire))
    }

    /// Starts or resumes; from the end it starts again from the beginning.
    pub fn play(&self) {
        let ended = self.snapshot().state == PlayerState::Ended;
        self.audio.play();
        if ended {
            self.send_seek(0);
        } else if let Some(video) = &self.video {
            let _ = video.commands.send(Command::Wake);
        }
    }

    pub fn pause(&self) {
        self.audio.pause();
    }

    pub fn seek(&self, ms: u64) {
        self.audio.seek(ms);
        self.send_seek(ms);
    }

    fn send_seek(&self, ms: u64) {
        if let Some(video) = &self.video {
            let epoch = self.clock.epoch() + 1;
            let _ = video.commands.send(Command::Seek { ms, epoch });
        }
    }

    pub fn set_volume(&self, volume: f32) {
        self.audio.set_volume(volume);
    }

    pub fn set_muted(&self, muted: bool) {
        self.audio.set_muted(muted);
    }
}

fn read_head(file: &mut File, head: &mut [u8; 8]) -> Result<usize, StartError> {
    let mut done = 0;
    while done < head.len() {
        match file.read(&mut head[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return Err(StartError::Damaged),
        }
    }
    file.rewind().map_err(|_| StartError::Damaged)?;
    Ok(done)
}

/// The audio track of a container as a [`PacketSource`]: Opus packets as stored, AAC-LC
/// frames decoded and re-encoded by an [`AacTranscoder`].
struct ContainerAudio {
    media: Arc<Media>,
    reader: BufReader<File>,
    next: usize,
    buffer: Vec<u8>,
    aac: Option<AacTranscoder>,
}

impl ContainerAudio {
    /// The source for `media`'s audio track, or the line to show when it cannot be played
    /// (the first packet is not 20 ms Opus, or the first AAC frame does not decode).
    fn open(media: &Arc<Media>, reader: BufReader<File>) -> Result<Self, &'static str> {
        let track = media.audio.as_ref().ok_or(OTHER_UNSUPPORTED)?;
        let aac = match track.codec {
            AudioCodecId::Opus => None,
            AudioCodecId::Aac => {
                Some(AacTranscoder::new(&track.config, track.rate).map_err(|_| AAC_UNSUPPORTED)?)
            }
        };
        let mut source = Self {
            media: Arc::clone(media),
            reader,
            next: 0,
            buffer: Vec::new(),
            aac,
        };
        if source.aac.is_some() {
            let first = source.read_at(0).map_err(|_| AAC_UNSUPPORTED)?;
            let decoded = first.is_some_and(|_| {
                let frame = std::mem::take(&mut source.buffer);
                let ok = source
                    .aac
                    .as_mut()
                    .is_some_and(|aac| aac.push(&frame, false));
                source.buffer = frame;
                ok
            });
            if !decoded {
                return Err(AAC_UNSUPPORTED);
            }
            source.restart(0);
        } else if !matches!(source.opus_at(0), Ok(Some(_))) {
            return Err(OTHER_UNSUPPORTED);
        }
        Ok(source)
    }

    /// Loads the stored bytes of sample `index` into `buffer`; `None` past the end.
    fn read_at(&mut self, index: usize) -> Result<Option<()>, PlayerFailure> {
        let Some(sample) = self
            .media
            .audio
            .as_ref()
            .and_then(|track| track.samples.get(index))
            .copied()
        else {
            return Ok(None);
        };
        container::read_sample(&mut self.reader, &sample, &mut self.buffer)
            .map_err(|_| PlayerFailure::Damaged)?;
        Ok(Some(()))
    }

    fn opus_at(&mut self, index: usize) -> Result<Option<EncodedPacket>, PlayerFailure> {
        if self.read_at(index)?.is_none() {
            return Ok(None);
        }
        if packet_samples(&self.buffer) != Some(fastcord_audio::codec::FRAME_SAMPLES) {
            return Err(PlayerFailure::Unsupported);
        }
        EncodedPacket::from_slice(&self.buffer)
            .map(Some)
            .map_err(|_| PlayerFailure::Unsupported)
    }

    /// Positions the stream at sample `index`. AAC decodes the frame before it first, to
    /// prime the overlap-add, and drops that audio.
    fn restart(&mut self, index: usize) {
        self.next = index;
        let Some(aac) = self.aac.as_mut() else {
            return;
        };
        aac.reset();
        if let Some(prime) = index.checked_sub(1)
            && self.read_at(prime).is_ok_and(|read| read.is_some())
            && let Some(aac) = self.aac.as_mut()
        {
            aac.push(&self.buffer, true);
        }
    }
}

impl PacketSource for ContainerAudio {
    fn duration_ms(&self) -> Option<u64> {
        Some(self.media.duration_ms)
    }

    fn next_packet(&mut self) -> Result<Option<EncodedPacket>, PlayerFailure> {
        if self.aac.is_none() {
            let packet = self.opus_at(self.next)?;
            if packet.is_some() {
                self.next += 1;
            }
            return Ok(packet);
        }
        // One AAC frame yields about 21 ms of audio; pop everything before reading more.
        loop {
            if let Some(packet) = self.aac.as_mut().and_then(AacTranscoder::pop) {
                return Ok(Some(packet));
            }
            if self.read_at(self.next)?.is_none() {
                return Ok(None);
            }
            self.next += 1;
            if let Some(aac) = self.aac.as_mut() {
                aac.push(&self.buffer, false);
            }
        }
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), PlayerFailure> {
        let index = self
            .media
            .audio
            .as_ref()
            .map_or(0, |track| track.index_at(ms));
        self.restart(index);
        Ok(())
    }
}

fn start_video(
    media: Arc<Media>,
    reader: BufReader<File>,
    clock: PlayerClock,
    open: OpenDecoder,
) -> Result<VideoWorker, StartError> {
    let track = media.video.as_ref().expect("checked by the caller");
    let (width, height) = (track.width, track.height);
    let codec = track.codec;
    let session = playback::Session::new(format!("attachment:{width}x{height}"));
    session.set_visible(true);
    let presented = Arc::new(AtomicU64::new(0));
    let (commands, receiver) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let worker = {
        let session = Arc::clone(&session);
        let presented = Arc::clone(&presented);
        move || {
            let decoder = match open(codec) {
                Ok(decoder) => decoder,
                Err(_) => {
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            session.status(Status::Playing(decoder.backend().to_string()));
            let _ = ready_tx.send(true);
            video_thread(Worker {
                media,
                reader,
                decoder,
                clock,
                session,
                commands: receiver,
                presented,
            });
        }
    };
    std::thread::Builder::new()
        .name("fastcord-video-player".to_owned())
        .spawn(worker)
        .map_err(|_| StartError::Decoder)?;
    if !ready_rx.recv().unwrap_or(false) {
        session.close();
        return Err(StartError::Decoder);
    }
    Ok(VideoWorker {
        commands,
        session,
        size: (width, height),
        #[cfg(test)]
        presented,
    })
}

struct Worker {
    media: Arc<Media>,
    reader: BufReader<File>,
    decoder: Box<dyn VideoDecoder>,
    clock: PlayerClock,
    session: Arc<playback::Session>,
    commands: Receiver<Command>,
    presented: Arc<AtomicU64>,
}

/// What the worker waits for after a command, because the audio thread acts on its own.
#[derive(Clone, Copy)]
enum Settle {
    Epoch(u64),
    Playing,
}

/// Where the worker is in the stream.
struct Position {
    pool: FramePool,
    /// Decoded pictures not yet due, oldest first.
    pending: VecDeque<(u64, FrameLease)>,
    /// Next sample to decode.
    next: usize,
    /// A picture has been shown since the last (re)start.
    shown: bool,
    /// Every sample was decoded and the decoder flushed.
    finished: bool,
    /// The decoder gave up; nothing more is attempted until a seek.
    failed: bool,
    failures: u32,
    /// Drop delta frames until a keyframe (after a decode error).
    need_key: bool,
    settle: Option<(Settle, Instant)>,
}

impl Position {
    fn apply(&mut self, command: Command, w: &mut Worker, track: &VideoTrack) {
        match command {
            Command::Wake => self.settle = Some((Settle::Playing, Instant::now() + SETTLE)),
            Command::Seek { ms, epoch } => {
                self.next = track.keyframe_for(ms);
                self.pending.clear();
                let _ = w.decoder.flush(&mut |_| {});
                self.shown = false;
                self.finished = false;
                self.failed = false;
                self.failures = 0;
                self.need_key = false;
                w.session
                    .status(Status::Playing(w.decoder.backend().to_string()));
                self.settle = Some((Settle::Epoch(epoch), Instant::now() + SETTLE));
            }
        }
    }

    /// Queues a decoded picture. A picture that is already due makes older due ones obsolete.
    fn queue(&mut self, picture: DecodedFrame<'_>, now: u64) {
        let pts = u64::try_from(picture.timestamp.as_millis()).unwrap_or(u64::MAX);
        let Ok(lease) = self.pool.lease(picture) else {
            return;
        };
        if pts <= now {
            self.pending.retain(|(older, _)| *older > now);
        }
        if self.pending.len() >= MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back((pts, lease));
    }
}

fn video_thread(mut w: Worker) {
    let media = Arc::clone(&w.media);
    let track = media.video.as_ref().expect("a video worker has a track");
    let mut at = Position {
        pool: FramePool::new(crate::render::frame_format()),
        pending: VecDeque::new(),
        next: 0,
        shown: false,
        finished: false,
        failed: false,
        failures: 0,
        need_key: false,
        settle: None,
    };
    let (mut stored, mut unit) = (Vec::new(), Vec::new());
    loop {
        loop {
            match w.commands.try_recv() {
                Ok(command) => at.apply(command, &mut w, track),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        let snapshot = w.clock.snapshot();
        if let Some((what, deadline)) = at.settle {
            let done = match what {
                Settle::Epoch(epoch) => w.clock.epoch() >= epoch,
                Settle::Playing => snapshot.state != PlayerState::Paused,
            };
            if done || Instant::now() >= deadline {
                at.settle = None;
            } else {
                match w.commands.recv_timeout(Duration::from_millis(2)) {
                    Ok(command) => at.apply(command, &mut w, track),
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
                continue;
            }
        }
        let now = snapshot.position_ms;
        let playing = snapshot.state == PlayerState::Playing;

        // Show the newest picture whose time has come; with nothing shown yet and the clock
        // not running, show the first one as a still.
        let mut due = None;
        while at.pending.front().is_some_and(|(pts, _)| *pts <= now) {
            due = at.pending.pop_front();
        }
        if due.is_none() && !at.shown && !playing {
            due = at.pending.pop_front();
        }
        if let Some((_, lease)) = due {
            w.session.push(lease);
            w.presented.fetch_add(1, Ordering::AcqRel);
            at.shown = true;
        }

        if at.pending.is_empty() && (playing || !at.shown) && !at.finished && !at.failed {
            decode_step(&mut w, track, &mut at, now, &mut stored, &mut unit);
            continue;
        }

        // Nothing to do until the next picture is due or a command arrives; paused, finished
        // or failed, only a command can change that, so block.
        let command = match at.pending.front() {
            Some((pts, _)) if playing => {
                let wait = Duration::from_millis(pts.saturating_sub(now).max(1)).min(MAX_SLEEP);
                match w.commands.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            _ => match w.commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
        };
        if let Some(command) = command {
            at.apply(command, &mut w, track);
        }
    }
}

/// Decodes the next sample (or flushes at the end), queueing the pictures it yields.
fn decode_step(
    w: &mut Worker,
    track: &VideoTrack,
    at: &mut Position,
    now: u64,
    stored: &mut Vec<u8>,
    unit: &mut Vec<u8>,
) {
    let Some(sample) = track.samples.get(at.next).copied() else {
        let flushed = w.decoder.flush(&mut |picture| at.queue(picture, now));
        at.finished = true;
        at.failed |= flushed.is_err();
        return;
    };
    at.next += 1;
    if at.need_key && !sample.key {
        return;
    }
    let result = container::read_sample(&mut w.reader, &sample, stored)
        .and_then(|()| track.access_unit(stored, sample.key, unit))
        .map_err(|_| true)
        .and_then(|()| {
            let timestamp = Duration::from_millis(sample.pts_ms.into());
            w.decoder
                .decode(unit, timestamp, &mut |picture| at.queue(picture, now))
                .map_err(|error| {
                    // A damaged frame is skipped; a decoder that cannot work at all is not.
                    !matches!(
                        error,
                        CodecError::InvalidBitstream(_) | CodecError::Platform { .. }
                    )
                })
        });
    match result {
        Ok(()) => {
            at.failures = 0;
            at.need_key = false;
        }
        Err(fatal) => {
            at.failures += 1;
            at.need_key = true;
            let _ = w.decoder.flush(&mut |_| {});
            if fatal || at.failures >= MAX_DECODE_FAILURES {
                at.failed = true;
                at.pending.clear();
                w.session
                    .status(Status::Failed("this video could not be decoded".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEBM: &[u8] = include_bytes!("../../../fixtures/video/container-av1-opus.webm");
    const MP4_AAC_LC: &[u8] = include_bytes!("../../../fixtures/video/container-h264-aac-lc.mp4");
    const MP4_AAC: &[u8] = include_bytes!("../../../fixtures/video/container-h264-aac.mp4");

    /// A fixture written to a private temporary file, removed on drop.
    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir()
                .join(format!("fastcord-av-test-{}-{name}", std::process::id()));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn software() -> OpenDecoder {
        Box::new(|codec| match codec {
            VideoCodecId::Av1 => Ok(Box::new(fastcord_video::av1::Decoder::open()?)),
            VideoCodecId::H264 => {
                fastcord_video::openh264::open_decoder(fastcord_video::BackendPreference::Software)
                    .map(|selected| Box::new(selected.codec) as Box<dyn VideoDecoder>)
                    .map_err(|_| CodecError::PlatformUnavailable("OpenH264"))
            }
        })
    }

    fn wait_for(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < deadline, "timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_webm_plays_through_shows_pictures_and_replays() {
        let file = TempFile::new("a.webm", WEBM);
        let player = MediaPlayer::start_with(&file.0, None, software()).unwrap();
        let (session, size) = player.video().unwrap();
        assert_eq!(size, (35, 27));
        assert_eq!(player.snapshot().duration_ms, Some(120));
        wait_for(|| player.snapshot().state == PlayerState::Ended);
        wait_for(|| player.presented() >= 2);
        let frame = session.frame().expect("the last picture stays up");
        assert_eq!((frame.width(), frame.height()), (35, 27));
        // Replay from the end restarts the video as well.
        let before = player.presented();
        player.play();
        wait_for(|| player.presented() > before);
        wait_for(|| player.snapshot().state == PlayerState::Ended);
    }

    #[test]
    fn seeking_while_ended_shows_a_still_without_playing() {
        let file = TempFile::new("seek.webm", WEBM);
        let player = MediaPlayer::start_with(&file.0, None, software()).unwrap();
        wait_for(|| player.snapshot().state == PlayerState::Ended);
        wait_for(|| player.presented() >= 2);
        let before = player.presented();
        player.seek(40);
        wait_for(|| player.presented() > before);
        let snapshot = player.snapshot();
        assert_eq!(snapshot.state, PlayerState::Paused);
        assert_eq!(snapshot.position_ms, 40);
        // Paused: nothing more is decoded or presented.
        let still = player.presented();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(player.presented(), still);
    }

    #[test]
    fn h264_with_aac_plays_on_the_silent_clock_without_an_output() {
        let file = TempFile::new("aac.mp4", MP4_AAC_LC);
        let player = MediaPlayer::start_with(&file.0, None, software()).unwrap();
        assert!(player.notice().is_none());
        wait_for(|| player.presented() >= 1);
        wait_for(|| player.snapshot().state == PlayerState::Ended);
        assert!(!player.video_failed());
    }

    #[test]
    fn dropping_the_player_closes_the_video_slot() {
        let file = TempFile::new("drop.webm", WEBM);
        let player = MediaPlayer::start_with(&file.0, None, software()).unwrap();
        let (session, _) = player.video().unwrap();
        assert!(session.active.load(Ordering::Acquire));
        drop(player);
        assert!(!session.active.load(Ordering::Acquire));
        assert!(session.frame().is_none());
    }

    #[test]
    fn files_that_cannot_play_fail_with_a_reason() {
        let vp9 = {
            let at = WEBM.windows(5).position(|w| w == b"V_AV1").unwrap();
            let mut bytes = WEBM.to_vec();
            bytes[at..at + 5].copy_from_slice(b"V_VP9");
            TempFile::new("vp9.webm", &bytes)
        };
        assert_eq!(
            MediaPlayer::start_with(&vp9.0, None, software()).unwrap_err(),
            StartError::UnsupportedVideo
        );
        let junk = TempFile::new("junk.mp4", b"   ftypisom   isom");
        assert_eq!(
            MediaPlayer::start_with(&junk.0, None, software()).unwrap_err(),
            StartError::Damaged
        );
        let no_decoder = TempFile::new("nodec.webm", WEBM);
        let refuse: OpenDecoder = Box::new(|_| Err(CodecError::PlatformUnavailable("test")));
        assert_eq!(
            MediaPlayer::start_with(&no_decoder.0, None, refuse).unwrap_err(),
            StartError::Decoder
        );
        assert_eq!(
            MediaPlayer::start_with(
                &std::env::temp_dir().join("fastcord-missing"),
                None,
                software()
            )
            .unwrap_err(),
            StartError::Damaged
        );
    }

    fn audio_source(bytes: &[u8], name: &str) -> (TempFile, Result<ContainerAudio, &'static str>) {
        let file = TempFile::new(name, bytes);
        let media =
            Arc::new(container::open(&mut BufReader::new(File::open(&file.0).unwrap())).unwrap());
        let source = ContainerAudio::open(&media, BufReader::new(File::open(&file.0).unwrap()));
        (file, source)
    }

    #[test]
    fn the_opus_track_is_served_as_packets_and_seeks_to_a_packet() {
        let (_file, source) = audio_source(WEBM, "audio.webm");
        let mut source = source.unwrap();
        assert_eq!(source.duration_ms(), Some(120));
        let ids: Vec<u8> = std::iter::from_fn(|| source.next_packet().unwrap())
            .map(|packet| packet.as_bytes()[3])
            .collect();
        assert_eq!(ids, [0, 1, 2, 3, 4, 5]);
        source.seek_ms(45).unwrap();
        assert_eq!(source.next_packet().unwrap().unwrap().as_bytes()[3], 3);
    }

    #[test]
    fn packets_that_are_not_20_ms_opus_are_refused() {
        // Turn every packet's table-of-contents byte into a 10 ms CELT frame.
        let mut bytes = WEBM.to_vec();
        for at in 0..bytes.len() - 4 {
            if bytes[at..at + 3] == [0xF8, 0xFF, 0xFE] {
                bytes[at] = 0xF0;
            }
        }
        let (_file, source) = audio_source(&bytes, "ten-ms.webm");
        assert_eq!(source.err(), Some(OTHER_UNSUPPORTED));
    }

    /// Power of `samples` (one channel of `stride`-interleaved audio) at `hz`, relative to
    /// the signal's own power.
    fn tone_share(samples: &[f32], stride: usize, channel: usize, hz: f32) -> f32 {
        let (mut re, mut im, mut total) = (0.0f32, 0.0f32, 0.0f32);
        for (i, frame) in samples.chunks_exact(stride).enumerate() {
            let x = frame[channel];
            let phase = 2.0 * std::f32::consts::PI * hz * i as f32 / 48_000.0;
            re += x * phase.cos();
            im += x * phase.sin();
            total += x * x;
        }
        let n = (samples.len() / stride) as f32;
        2.0 * (re * re + im * im) / (n * total.max(1e-9))
    }

    fn decode_all(source: &mut ContainerAudio) -> Vec<f32> {
        let mut decoder = fastcord_audio::VoiceDecoder::new().unwrap();
        let mut pcm = Vec::new();
        let mut out = vec![0.0; fastcord_audio::codec::MAX_FRAME_SAMPLES * 2];
        while let Some(packet) = source.next_packet().unwrap() {
            let frames = decoder.decode(packet.as_bytes(), &mut out).unwrap();
            pcm.extend_from_slice(&out[..frames * 2]);
        }
        pcm
    }

    #[test]
    fn a_real_aac_lc_track_is_decoded_to_the_tone_it_holds() {
        let (_file, source) = audio_source(MP4_AAC_LC, "lc.mp4");
        let mut source = source.unwrap();
        let pcm = decode_all(&mut source);
        // 21 frames of 1024 samples at 44.1 kHz resampled to 48 kHz, in 20 ms packets.
        assert!(pcm.len() / 2 >= 22_000, "{} frames", pcm.len() / 2);
        // Skip the codec start-up and look at the steady middle: left is 440 Hz, right 880 Hz.
        let middle = &pcm[2 * 6_000..2 * 18_000];
        assert!(tone_share(middle, 2, 0, 440.0) > 0.9);
        assert!(tone_share(middle, 2, 1, 880.0) > 0.9);
        assert!(tone_share(middle, 2, 0, 880.0) < 0.05);
    }

    #[test]
    fn seeking_aac_restarts_at_a_frame_and_still_decodes_the_tone() {
        let (_file, source) = audio_source(MP4_AAC_LC, "lc-seek.mp4");
        let mut source = source.unwrap();
        assert!(source.next_packet().unwrap().is_some());
        source.seek_ms(250).unwrap();
        assert_eq!(source.next, 11);
        let pcm = decode_all(&mut source);
        // Frames 11..=20 remain: about 240 ms.
        assert!(
            (10_000..13_000).contains(&(pcm.len() / 2)),
            "{}",
            pcm.len() / 2
        );
        assert!(tone_share(&pcm[2 * 2_000..], 2, 0, 440.0) > 0.9);
        source.seek_ms(0).unwrap();
        assert!(source.next_packet().unwrap().is_some());
    }

    #[test]
    fn aac_frames_that_do_not_decode_leave_the_video_silent_with_a_note() {
        // The older fixture has a valid configuration but payloads that are not AAC.
        let (_file, source) = audio_source(MP4_AAC, "fake-aac.mp4");
        assert_eq!(source.err(), Some(AAC_UNSUPPORTED));
    }

    #[test]
    fn a_damaged_aac_frame_in_the_middle_becomes_silence_not_a_failure() {
        let mut bytes = MP4_AAC_LC.to_vec();
        let (_file, source) = audio_source(&bytes, "lc-ok.mp4");
        let frames_ok = decode_all(&mut source.unwrap()).len();
        // Overwrite audio frame 10 with 0xFF bytes.
        let media = container::open(&mut std::io::Cursor::new(&bytes)).unwrap();
        let frame = media.audio.unwrap().samples[10];
        let at = frame.offset as usize;
        bytes[at..at + frame.size as usize].fill(0xFF);
        let (_file, source) = audio_source(&bytes, "lc-bad.mp4");
        let frames_bad = decode_all(&mut source.unwrap()).len();
        // The damaged frame is replaced by silence of its length (within one packet).
        assert!(
            frames_ok.abs_diff(frames_bad) <= 2 * 960,
            "{frames_ok} {frames_bad}"
        );
    }
}
