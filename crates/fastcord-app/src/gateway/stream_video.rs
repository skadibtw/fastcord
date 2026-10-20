//! Decoding of a watched stream's video (SPEC §8.2, §8.5; milestone 28).
//!
//! The session supplies complete, DAVE-opened H.264 access units or AV1
//! temporal units. A dedicated thread starts only for a visible keyframe and
//! opens the negotiated decoder there: Media Foundation on Windows,
//! VideoToolbox on macOS, or dav1d for AV1. Linux H.264 is unavailable until
//! M30 and is not offered by the watch session.
//!
//! The encoded queue is bounded by frames and bytes. A lost dependency breaks
//! the chain, discards deltas, and requests one keyframe. Consecutive failing
//! keyframes cause at most three requests. Codec/SSRC changes and suspension
//! discard the queue and release decoder references on the owning thread.
//! Resuming requests a keyframe once, without opening a decoder beforehand.
//!
//! Pictures are borrowed by the sink on the decoding thread. Visibility and
//! a generation check suppress output from a decode already in flight when
//! the stream is hidden, replaced, or ended. Native calls cannot be cancelled
//! mid-frame, but no further frame is submitted while hidden. Failures and
//! backend choices are reported through a lightweight status callback.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use fastcord_media::video::{ReceivedVideo, VideoCodec};
use fastcord_media::{MediaSendError, MediaSender};
use fastcord_video::{CodecError, DecodedFrame, VideoDecoder};
use parking_lot::{Condvar, Mutex, MutexGuard};

/// Frames waiting for the decoder: at 30 fps about 130 ms of delay at most.
const MAX_QUEUED_FRAMES: usize = 4;
/// Bytes waiting for the decoder (a frame is at most 2 MiB, so three of them).
pub(crate) const MAX_QUEUED_BYTES: usize = 6 << 20;
/// Keyframes that failed to decode, in a row, that are answered with a request.
const MAX_FAILED_KEYFRAMES: u32 = 3;
/// The RTP clock of video.
const RTP_CLOCK_HZ: u64 = 90_000;

/// Opens the selected codec on the decoding thread.
pub(super) type OpenDecoder =
    Box<dyn FnMut(VideoCodec) -> Result<Box<dyn VideoDecoder>, CodecError> + Send>;
/// Asks the stream's sender for a keyframe (PLI) for an SSRC. Never blocks.
pub(super) type RequestKeyframe = Arc<dyn Fn(u32) -> Result<(), MediaSendError> + Send + Sync>;
/// Borrows each decoded picture, with the SSRC of its stream, on the decoding
/// thread.
pub type Sink = Box<dyn FnMut(u32, DecodedFrame<'_>) + Send>;
/// Publishes state changes, never pictures; called from the session or worker.
pub type StatusSink = Arc<dyn Fn(StreamStatus) + Send + Sync>;

/// State for the actual watch UI. Failures retain a concrete backend/session
/// explanation rather than silently leaving a permanently empty frame slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamStatus {
    Connecting,
    Decoding { backend: String },
    Suspended,
    Failed(String),
    Ended,
}

/// What the decoding thread needs; all decoder/sink work moves to that thread.
pub(super) struct Config {
    pub(super) open: OpenDecoder,
    pub(super) request_keyframe: RequestKeyframe,
    pub(super) visible: Arc<AtomicBool>,
    pub(super) sink: Option<Sink>,
    pub(super) status: StatusSink,
}

/// Counters of one stream's decoding. During normal operation, every frame
/// offered ends up in exactly one of `frames_dropped` and `frames_processed`.
#[cfg(any(test, debug_assertions))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DecodeStats {
    /// Encoded frames offered by the session.
    pub(super) frames_received: u64,
    /// Frames discarded without reaching the decoder: the queue was full, the
    /// stream waited for a keyframe, or the decoding thread had ended.
    pub(super) frames_dropped: u64,
    /// Frames the decoding thread took off the queue, decoded or not.
    pub(super) frames_processed: u64,
    /// Pictures the decoder produced.
    pub(super) pictures_decoded: u64,
    /// Processed frames that did not decode: the decoder refused them, could
    /// not open, or they continued no stream it had.
    pub(super) decode_errors: u64,
    /// Processed frames whose codec backend is unavailable on this platform.
    pub(super) frames_unsupported: u64,
    /// Keyframe requests the voice session accepted.
    pub(super) keyframes_requested: u64,
    /// The latest picture's size.
    pub(super) width: u32,
    pub(super) height: u32,
    /// The decoder in use, once one opened.
    pub(super) backend: Option<String>,
}

#[derive(Default)]
struct Counters {
    frames_received: AtomicU64,
    frames_dropped: AtomicU64,
    frames_processed: AtomicU64,
    pictures_decoded: AtomicU64,
    decode_errors: AtomicU64,
    frames_unsupported: AtomicU64,
    keyframes_requested: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
}

impl Counters {
    fn add(counter: &AtomicU64, by: usize) {
        counter.fetch_add(by as u64, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stream {
    codec: VideoCodec,
    ssrc: u32,
}

struct Queue {
    frames: VecDeque<ReceivedVideo>,
    bytes: usize,
    /// No decodable chain exists: delta frames are dropped until a keyframe is
    /// queued. True before the first keyframe.
    awaiting_keyframe: bool,
    /// Identity of the latest received stream, including while hidden.
    stream: Option<Stream>,
    last_ssrc: Option<u32>,
    /// Invalidates a frame already in flight when its references are released.
    generation: u64,
    suspended: bool,
    /// Resume/change needs one PLI once an SSRC is known.
    request_needed: bool,
    /// A keyframe request the voice session could not take yet.
    retry_request: Option<u32>,
    /// The owner thread must discard its decoder before another frame.
    release_decoder: bool,
    /// The session ended or the decoding thread did.
    closed: bool,
}

impl Queue {
    /// Empties the queue and says how many frames that dropped.
    fn clear(&mut self) -> usize {
        let dropped = self.frames.len();
        self.frames.clear();
        self.bytes = 0;
        dropped
    }
    fn reset(&mut self) -> usize {
        self.generation = self.generation.wrapping_add(1);
        self.release_decoder = true;
        self.awaiting_keyframe = true;
        self.retry_request = None;
        self.request_needed = false;
        self.clear()
    }

    fn pop(&mut self) -> Option<ReceivedVideo> {
        let frame = self.frames.pop_front()?;
        self.bytes -= frame.data.len();
        Some(frame)
    }
}

struct Shared {
    queue: Mutex<Queue>,
    wake: Condvar,
    counters: Counters,
    backend: Mutex<Option<String>>,
    request_keyframe: RequestKeyframe,
    visible: Arc<AtomicBool>,
    status: StatusSink,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock()
    }
    fn current(&self, generation: u64) -> bool {
        if !self.visible.load(Ordering::Acquire) {
            return false;
        }
        let queue = self.lock();
        !queue.closed && !queue.suspended && queue.generation == generation
    }

    /// Asks the sender for a keyframe. A full control queue is retried with the
    /// next frame, never forgotten.
    fn request(&self, queue: &mut Queue, ssrc: u32) {
        if queue.suspended || queue.closed || !self.visible.load(Ordering::Acquire) {
            return;
        }
        queue.request_needed = false;
        match (self.request_keyframe)(ssrc) {
            Ok(()) => {
                queue.retry_request = None;
                Counters::add(&self.counters.keyframes_requested, 1);
            }
            Err(MediaSendError::Full) => queue.retry_request = Some(ssrc),
            Err(_) => queue.retry_request = None,
        }
    }
}

/// The session side of a stream's decoding. Nothing runs before a visible
/// keyframe; dropping it ends the thread, which frees the decoder.
pub(super) struct StreamDecoder {
    shared: Arc<Shared>,
    pending: Option<(OpenDecoder, Option<Sink>)>,
    thread: Option<JoinHandle<()>>,
}

impl StreamDecoder {
    /// Native H.264 or dav1d AV1, with keyframes asked for through `media`.
    pub(super) fn new(
        media: MediaSender,
        visible: Arc<AtomicBool>,
        sink: Option<Sink>,
        status: StatusSink,
    ) -> Self {
        Self::with(Config {
            open: Box::new(open_decoder),
            request_keyframe: Arc::new(move |ssrc| media.request_video_keyframe(ssrc)),
            visible,
            sink,
            status,
        })
    }

    pub(super) fn with(config: Config) -> Self {
        let suspended = !config.visible.load(Ordering::Acquire);
        if suspended {
            (config.status)(StreamStatus::Suspended);
        }
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    frames: VecDeque::new(),
                    bytes: 0,
                    awaiting_keyframe: true,
                    stream: None,
                    last_ssrc: None,
                    generation: 0,
                    suspended,
                    request_needed: false,
                    retry_request: None,
                    release_decoder: false,
                    closed: false,
                }),
                wake: Condvar::new(),
                counters: Counters::default(),
                backend: Mutex::new(None),
                request_keyframe: config.request_keyframe,
                visible: config.visible,
                status: config.status,
            }),
            pending: Some((config.open, config.sink)),
            thread: None,
        }
    }

    #[cfg(any(test, debug_assertions))]
    pub(super) fn stats(&self) -> DecodeStats {
        let counters = &self.shared.counters;
        // Loaded first, with Acquire: whatever the thread did for the frames it
        // counts as processed is visible to the loads below.
        let frames_processed = counters.frames_processed.load(Ordering::Acquire);
        DecodeStats {
            frames_received: counters.frames_received.load(Ordering::Relaxed),
            frames_dropped: counters.frames_dropped.load(Ordering::Relaxed),
            frames_processed,
            pictures_decoded: counters.pictures_decoded.load(Ordering::Relaxed),
            decode_errors: counters.decode_errors.load(Ordering::Relaxed),
            frames_unsupported: counters.frames_unsupported.load(Ordering::Relaxed),
            keyframes_requested: counters.keyframes_requested.load(Ordering::Relaxed),
            width: counters.width.load(Ordering::Relaxed),
            height: counters.height.load(Ordering::Relaxed),
            backend: self.shared.backend.lock().clone(),
        }
    }

    /// Applies the shared gate even if no media is arriving. The session's
    /// visibility watch wakes this method; the atomic also gates in-flight work.
    pub(super) fn visibility_changed(&self) {
        if !self.shared.visible.load(Ordering::Acquire) {
            self.suspend();
            return;
        }
        let mut queue = self.shared.lock();
        if !queue.suspended || queue.closed {
            return;
        }
        queue.suspended = false;
        queue.request_needed = true;
        if let Some(ssrc) = queue.last_ssrc {
            self.shared.request(&mut queue, ssrc);
        }
        drop(queue);
        (self.shared.status)(StreamStatus::Connecting);
    }

    /// The watch publishes only actual changes. Equal successive observed
    /// values therefore include a coalesced hide/show pulse, which must still
    /// invalidate references before decoding resumes.
    pub(super) fn visibility_notified(&self, previous: bool, announced: bool) {
        if previous || !announced {
            self.suspend();
        }
        self.visibility_changed();
    }

    /// Clears pending dependencies now and schedules native release on its
    /// owner, including when a hide/show notification was coalesced.
    pub(super) fn suspend(&self) {
        let mut queue = self.shared.lock();
        if queue.suspended || queue.closed {
            return;
        }
        queue.suspended = true;
        let dropped = queue.reset();
        drop(queue);
        Counters::add(&self.shared.counters.frames_dropped, dropped);
        self.shared.wake.notify_one();
        (self.shared.status)(StreamStatus::Suspended);
    }

    /// Remembers source announcements, allowing a resume PLI before any frame
    /// has arrived. An inactive/replaced source also frees stale references.
    pub(super) fn source_changed(&self, ssrc: u32) {
        let source = (ssrc != 0).then_some(ssrc);
        let mut queue = self.shared.lock();
        if queue.closed || queue.last_ssrc == source {
            return;
        }
        let replaced = queue.last_ssrc.is_some();
        let resume_request = queue.request_needed;
        let dropped = queue.reset();
        queue.stream = None;
        queue.last_ssrc = source;
        queue.request_needed = (replaced || resume_request) && source.is_some();
        if resume_request && let Some(ssrc) = source {
            self.shared.request(&mut queue, ssrc);
        }
        drop(queue);
        Counters::add(&self.shared.counters.frames_dropped, dropped);
        self.shared.wake.notify_one();
    }

    /// Offers one frame without doing codec work or waiting for the worker.
    pub(super) fn push(&mut self, frame: ReceivedVideo) {
        let shared = Arc::clone(&self.shared);
        let counters = &shared.counters;
        Counters::add(&counters.frames_received, 1);
        // Do not resume ahead of the watch notification: otherwise a
        // coalesced hide/show pulse could cause two resume requests.
        if !shared.visible.load(Ordering::Acquire) {
            self.suspend();
        }
        let stream = Stream {
            codec: frame.codec,
            ssrc: frame.ssrc,
        };
        let len = frame.data.len();
        let mut queue = shared.lock();
        if queue.closed {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        if queue.stream != Some(stream) {
            let replaced = queue.stream.is_some();
            let request_needed = queue.request_needed;
            let dropped = queue.reset();
            queue.stream = Some(stream);
            queue.last_ssrc = Some(stream.ssrc);
            queue.request_needed = replaced || request_needed;
            Counters::add(&counters.frames_dropped, dropped);
            shared.wake.notify_one();
        }
        if queue.suspended || !shared.visible.load(Ordering::Acquire) {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        // Even an oversized keyframe may not exceed the queue's byte bound.
        if len > MAX_QUEUED_BYTES {
            let was_waiting = queue.awaiting_keyframe;
            let dropped = queue.clear();
            queue.awaiting_keyframe = true;
            Counters::add(&counters.frames_dropped, dropped + 1);
            if !was_waiting {
                shared.request(&mut queue, stream.ssrc);
            }
            return;
        }
        if frame.keyframe {
            queue.retry_request = None;
            queue.request_needed = false;
        } else if let Some(retry) = queue.retry_request {
            shared.request(&mut queue, retry);
        } else if queue.request_needed {
            shared.request(&mut queue, stream.ssrc);
        }
        if queue.awaiting_keyframe && !frame.keyframe {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        if self.thread.is_none() {
            drop(queue);
            if !self.start_thread() {
                Counters::add(&counters.frames_dropped, 1);
                return;
            }
            queue = shared.lock();
            if queue.closed || queue.suspended || !shared.visible.load(Ordering::Acquire) {
                Counters::add(&counters.frames_dropped, 1);
                return;
            }
        }
        if queue.frames.len() >= MAX_QUEUED_FRAMES || queue.bytes + len > MAX_QUEUED_BYTES {
            let dropped = queue.clear();
            Counters::add(&counters.frames_dropped, dropped);
            if !frame.keyframe {
                Counters::add(&counters.frames_dropped, 1);
                queue.awaiting_keyframe = true;
                shared.request(&mut queue, stream.ssrc);
                return;
            }
        }
        if frame.keyframe {
            queue.awaiting_keyframe = false;
        }
        queue.bytes += len;
        queue.frames.push_back(frame);
        drop(queue);
        shared.wake.notify_one();
    }

    /// Starts the decoding thread; false when it cannot run.
    fn start_thread(&mut self) -> bool {
        let Some((open, sink)) = self.pending.take() else {
            return false;
        };
        let shared = Arc::clone(&self.shared);
        match std::thread::Builder::new()
            .name("fastcord-stream-decode".into())
            .spawn(move || Worker::new(shared, open, sink).run())
        {
            Ok(handle) => {
                self.thread = Some(handle);
                true
            }
            Err(error) => {
                self.shared.lock().closed = true;
                (self.shared.status)(StreamStatus::Failed(format!(
                    "Could not start video decoding worker: {error}"
                )));
                false
            }
        }
    }

    fn close(&self) {
        let mut queue = self.shared.lock();
        queue.closed = true;
        let dropped = queue.clear();
        drop(queue);
        Counters::add(&self.shared.counters.frames_dropped, dropped);
        self.shared.wake.notify_all();
    }

    /// Ends the decoding, waits for the thread (which frees the decoder), and
    /// returns the final counters.
    #[cfg(test)]
    pub(super) fn finish(mut self) -> DecodeStats {
        self.close();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.stats()
    }
}

impl Drop for StreamDecoder {
    fn drop(&mut self) {
        // The thread notices, frees its decoder, and ends; nothing waits for it
        // here, so a session task is never held up by a frame in the decoder.
        self.close();
    }
}

fn open_decoder(codec: VideoCodec) -> Result<Box<dyn VideoDecoder>, CodecError> {
    match codec {
        VideoCodec::Av1 => Ok(Box::new(fastcord_video::av1::Decoder::open()?)),
        VideoCodec::H264 => open_h264(),
    }
}

#[cfg(windows)]
pub(crate) fn open_h264() -> Result<Box<dyn VideoDecoder>, CodecError> {
    fastcord_video::media_foundation::open_decoder(fastcord_video::BackendPreference::Auto)
        .map(|selected| Box::new(selected.codec) as Box<dyn VideoDecoder>)
        .map_err(codec_open_error)
}

#[cfg(target_os = "macos")]
pub(crate) fn open_h264() -> Result<Box<dyn VideoDecoder>, CodecError> {
    fastcord_video::videotoolbox::open_decoder(fastcord_video::BackendPreference::Auto)
        .map(|selected| Box::new(selected.codec) as Box<dyn VideoDecoder>)
        .map_err(codec_open_error)
}

#[cfg(target_os = "linux")]
pub(crate) fn open_h264() -> Result<Box<dyn VideoDecoder>, CodecError> {
    // The VA-API hardware decoder first, the bundled OpenH264 software codec
    // when no device (or the relevant profile) exists (SPEC milestone 30).
    fastcord_video::vaapi::platform_decoder(fastcord_video::BackendPreference::Auto)
        .map(|selected| Box::new(selected.codec) as Box<dyn VideoDecoder>)
        .map_err(codec_open_error)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub(crate) fn open_h264() -> Result<Box<dyn VideoDecoder>, CodecError> {
    Err(CodecError::PlatformUnavailable(
        "H.264 decoding on this platform (Linux support requires M30)",
    ))
}

#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
fn codec_open_error(error: fastcord_video::OpenError) -> CodecError {
    match error {
        fastcord_video::OpenError::Invalid(error) | fastcord_video::OpenError::Discovery(error) => {
            error
        }
        fastcord_video::OpenError::Failed(mut attempts) => attempts
            .pop()
            .map(|attempt| attempt.error)
            .unwrap_or(CodecError::PlatformUnavailable("native H.264 decoder")),
        fastcord_video::OpenError::NoBackend(_) => {
            CodecError::PlatformUnavailable("native H.264 decoder")
        }
    }
}

/// Marks the stream closed when the decoding thread ends for any reason, so
/// the session stops queueing for a decoder that is gone.
struct CloseOnExit(Arc<Shared>);

impl Drop for CloseOnExit {
    fn drop(&mut self) {
        let mut queue = self.0.lock();
        queue.closed = true;
        let dropped = queue.clear();
        drop(queue);
        Counters::add(&self.0.counters.frames_dropped, dropped);
        if std::thread::panicking() {
            (self.0.status)(StreamStatus::Failed(
                "Video decoding worker stopped unexpectedly".into(),
            ));
        }
    }
}

struct Worker {
    shared: Arc<Shared>,
    open: OpenDecoder,
    sink: Option<Sink>,
    decoder: Option<Box<dyn VideoDecoder>>,
    failed_keyframes: u32,
    reported_backend: Option<fastcord_video::Backend>,
}

impl Worker {
    fn new(shared: Arc<Shared>, open: OpenDecoder, sink: Option<Sink>) -> Self {
        Self {
            shared,
            open,
            sink,
            decoder: None,
            failed_keyframes: 0,
            reported_backend: None,
        }
    }

    fn run(mut self) {
        let _exit = CloseOnExit(Arc::clone(&self.shared));
        while let Some((frame, generation)) = self.next() {
            self.process(frame, generation);
        }
        // The decoder and every picture it holds are freed here, on the thread
        // that opened them.
    }

    /// The next frame, waiting for one; `None` once the session ended.
    fn next(&mut self) -> Option<(ReceivedVideo, u64)> {
        loop {
            let mut queue = self.shared.lock();
            if queue.closed {
                return None;
            }
            if queue.release_decoder {
                queue.release_decoder = false;
                drop(queue);
                self.release();
                continue;
            }
            if !queue.suspended
                && self.shared.visible.load(Ordering::Acquire)
                && let Some(frame) = queue.pop()
            {
                return Some((frame, queue.generation));
            }
            self.shared.wake.wait(&mut queue);
        }
    }

    fn release(&mut self) {
        self.decoder = None;
        self.failed_keyframes = 0;
        self.reported_backend = None;
    }

    fn process(&mut self, frame: ReceivedVideo, generation: u64) {
        let shared = Arc::clone(&self.shared);
        let counters = &shared.counters;
        if !shared.current(generation) {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        if self.decode(&frame, generation).is_err() {
            self.fail(frame.ssrc, frame.keyframe, generation);
            counters.decode_errors.fetch_add(1, Ordering::Relaxed);
        } else if frame.keyframe {
            self.failed_keyframes = 0;
        }
        // Last: side effects are visible to whoever sees this counter.
        counters.frames_processed.fetch_add(1, Ordering::Release);
    }

    fn decode(&mut self, frame: &ReceivedVideo, generation: u64) -> Result<(), CodecError> {
        let shared = &self.shared;
        let decoder = match &mut self.decoder {
            Some(decoder) => decoder,
            empty => match (self.open)(frame.codec) {
                Ok(decoder) => empty.insert(decoder),
                Err(error) => {
                    if matches!(error, CodecError::PlatformUnavailable(_)) {
                        Counters::add(&shared.counters.frames_unsupported, 1);
                    }
                    if shared.current(generation) {
                        (shared.status)(StreamStatus::Failed(format!(
                            "Could not open {} decoder: {error}",
                            frame.codec.wire_name()
                        )));
                    }
                    return Err(error);
                }
            },
        };
        // Opening a native context can outlast a visibility/source change.
        if !shared.current(generation) {
            return Ok(());
        }
        let counters = &shared.counters;
        let sink = &mut self.sink;
        let ssrc = frame.ssrc;
        let timestamp =
            Duration::from_nanos(u64::from(frame.timestamp) * 1_000_000_000 / RTP_CLOCK_HZ);
        let mut pictures = 0;
        let result = decoder.decode(&frame.data, timestamp, &mut |picture| {
            pictures += 1;
            if !shared.current(generation) {
                return;
            }
            counters
                .width
                .store(picture.image.width(), Ordering::Relaxed);
            counters
                .height
                .store(picture.image.height(), Ordering::Relaxed);
            if let Some(sink) = sink.as_mut() {
                sink(ssrc, picture);
            }
        });
        counters
            .pictures_decoded
            .fetch_add(pictures, Ordering::Relaxed);
        if shared.current(generation) {
            match &result {
                Err(error) => (shared.status)(StreamStatus::Failed(format!(
                    "{} decode failed: {error}",
                    frame.codec.wire_name()
                ))),
                Ok(()) => {
                    let backend = decoder.backend();
                    if self.reported_backend.as_ref() != Some(backend) {
                        self.reported_backend = Some(backend.clone());
                        *shared.backend.lock() = Some(backend.name.clone());
                        (shared.status)(StreamStatus::Decoding {
                            backend: backend.to_string(),
                        });
                    }
                }
            }
        }
        if result.is_err() {
            self.reported_backend = None;
        }
        if let Err(CodecError::Platform { .. } | CodecError::Stalled(_)) = &result {
            // A native context that failed is not trusted again.
            self.decoder = None;
        }
        result
    }

    /// Drop failed dependencies up to a queued keyframe, or request one. A
    /// failure in an old/invisible generation cannot disturb its replacement.
    fn fail(&mut self, ssrc: u32, was_keyframe: bool, generation: u64) {
        let mut queue = self.shared.lock();
        if queue.closed
            || queue.suspended
            || queue.generation != generation
            || !self.shared.visible.load(Ordering::Acquire)
        {
            return;
        }
        if was_keyframe {
            self.failed_keyframes = self.failed_keyframes.saturating_add(1);
        }
        let mut dropped = 0;
        while queue.frames.front().is_some_and(|frame| !frame.keyframe) {
            queue.pop();
            dropped += 1;
        }
        if queue.frames.is_empty() {
            queue.awaiting_keyframe = true;
            if self.failed_keyframes <= MAX_FAILED_KEYFRAMES {
                self.shared.request(&mut queue, ssrc);
            }
        } else {
            queue.awaiting_keyframe = false;
        }
        drop(queue);
        Counters::add(&self.shared.counters.frames_dropped, dropped);
    }
}

#[cfg(test)]
mod tests;
