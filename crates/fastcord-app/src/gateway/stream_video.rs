//! Decoding of a watched stream's video (SPEC §8.2, §8.5; milestone 26).
//!
//! The voice session hands over complete, DAVE-opened frames. Decoding never
//! runs on the session task or the UI thread: [`StreamDecoder`] owns one
//! dedicated thread per stream session, started by the first AV1 keyframe (a
//! stream that never sends AV1 costs nothing), fed through a queue bounded in
//! frames and bytes, and ended with the session. The thread opens its decoder
//! only when that keyframe reaches it and frees it when the stream leaves AV1
//! or ends. Codec objects belong to the thread that opened them.
//!
//! Frames depend on their predecessors, so nothing is skipped silently. A
//! decoder error, a decoder that cannot keep up (the queue is full), or a stream
//! that changed under it breaks the chain: queued and arriving delta frames are
//! dropped, the sender is asked for a keyframe once
//! (`MediaSender::request_video_keyframe`, which also makes the voice driver
//! drop delta frames and send a PLI), and decoding resumes at the next
//! keyframe. Keyframes that fail to decode are asked for at most three times in
//! a row, so a stream the decoder cannot handle is never answered with a stream
//! of keyframes.
//!
//! Decoded pictures are counted and lent, for the duration of one call, to the
//! frame sink; milestone 28 installs the renderer's hand-off there, and until
//! then there is none, so they are discarded. Only AV1 is decoded here: the
//! platform H.264 decoders join with the renderer in milestone 28, and until
//! then H.264 frames are counted and ignored.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use fastcord_media::video::{ReceivedVideo, VideoCodec};
use fastcord_media::{MediaSendError, MediaSender};
use fastcord_video::{CodecError, DecodedFrame, VideoDecoder};
use parking_lot::{Condvar, Mutex, MutexGuard};

/// Frames waiting for the decoder: at 30 fps about 130 ms of delay at most.
const MAX_QUEUED_FRAMES: usize = 4;
/// Bytes waiting for the decoder (a frame is at most 2 MiB, so three of them).
const MAX_QUEUED_BYTES: usize = 6 << 20;
/// Keyframes that failed to decode, in a row, that are answered with a request.
const MAX_FAILED_KEYFRAMES: u32 = 3;
/// The RTP clock of video.
const RTP_CLOCK_HZ: u64 = 90_000;

/// Opens the decoder on the decoding thread.
pub(super) type OpenDecoder = Box<dyn FnMut() -> Result<Box<dyn VideoDecoder>, CodecError> + Send>;
/// Asks the stream's sender for a keyframe (PLI) for an SSRC. Never blocks.
pub(super) type RequestKeyframe = Arc<dyn Fn(u32) -> Result<(), MediaSendError> + Send + Sync>;
/// Borrows each decoded picture, with the SSRC of its stream, on the decoding
/// thread.
pub(super) type Sink = Box<dyn FnMut(u32, DecodedFrame<'_>) + Send>;

/// What the decoding thread needs; all of it moves to that thread.
pub(super) struct Config {
    pub(super) open: OpenDecoder,
    pub(super) request_keyframe: RequestKeyframe,
    pub(super) sink: Option<Sink>,
}

/// Counters of one stream's decoding. Every AV1 frame offered ends up in
/// exactly one of `frames_dropped` and `frames_processed`.
#[cfg(any(test, debug_assertions))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct DecodeStats {
    /// AV1 frames offered by the session.
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
    /// Frames of a codec without a decoder here (H.264 until milestone 28).
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

struct Queue {
    frames: VecDeque<ReceivedVideo>,
    bytes: usize,
    /// No decodable chain exists: delta frames are dropped until a keyframe is
    /// queued. True before the first keyframe.
    awaiting_keyframe: bool,
    /// A keyframe request the voice session could not take yet.
    retry_request: Option<u32>,
    /// The stream left AV1: the thread frees its decoder.
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
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock()
    }

    /// Asks the sender for a keyframe. A full control queue is retried with the
    /// next frame, never forgotten.
    fn request(&self, queue: &mut Queue, ssrc: u32) {
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

/// The session side of a stream's decoding. Nothing runs before the first AV1
/// keyframe; dropping it ends the thread, which frees the decoder.
pub(super) struct StreamDecoder {
    shared: Arc<Shared>,
    pending: Option<(OpenDecoder, Option<Sink>)>,
    thread: Option<JoinHandle<()>>,
}

impl StreamDecoder {
    /// A decoder for one stream session: dav1d for AV1, keyframes asked for
    /// through `media`.
    pub(super) fn new(media: MediaSender) -> Self {
        Self::with(Config {
            open: Box::new(open_av1),
            request_keyframe: Arc::new(move |ssrc| media.request_video_keyframe(ssrc)),
            sink: None,
        })
    }

    pub(super) fn with(config: Config) -> Self {
        Self {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    frames: VecDeque::new(),
                    bytes: 0,
                    awaiting_keyframe: true,
                    retry_request: None,
                    release_decoder: false,
                    closed: false,
                }),
                wake: Condvar::new(),
                counters: Counters::default(),
                backend: Mutex::new(None),
                request_keyframe: config.request_keyframe,
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

    /// Offers one frame for decoding without ever blocking the caller.
    pub(super) fn push(&mut self, frame: ReceivedVideo) {
        let shared = Arc::clone(&self.shared);
        let counters = &shared.counters;
        if frame.codec != VideoCodec::Av1 {
            Counters::add(&counters.frames_unsupported, 1);
            self.release_decoder();
            return;
        }
        Counters::add(&counters.frames_received, 1);
        // A stream starts at a keyframe: before one there is nothing to decode
        // with, so no thread and no decoder either.
        if self.thread.is_none() && (!frame.keyframe || !self.start_thread()) {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        let ssrc = frame.ssrc;
        let len = frame.data.len();
        let mut queue = shared.lock();
        if queue.closed {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        if frame.keyframe {
            // The keyframe being waited for is here.
            queue.retry_request = None;
        } else if let Some(retry) = queue.retry_request {
            shared.request(&mut queue, retry);
        }
        if queue.awaiting_keyframe && !frame.keyframe {
            Counters::add(&counters.frames_dropped, 1);
            return;
        }
        if queue.frames.len() >= MAX_QUEUED_FRAMES || queue.bytes + len > MAX_QUEUED_BYTES {
            // The decoder cannot keep up. What it has not reached is stale, and
            // a frame skipped breaks the chain; a keyframe restarts it.
            let dropped = queue.clear();
            Counters::add(&counters.frames_dropped, dropped);
            if !frame.keyframe {
                Counters::add(&counters.frames_dropped, 1);
                queue.awaiting_keyframe = true;
                shared.request(&mut queue, ssrc);
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

    /// The stream is no longer AV1: what the decoder holds describes nothing
    /// that will arrive, so it is freed and the next AV1 keyframe opens a new one.
    fn release_decoder(&self) {
        if self.thread.is_none() {
            return;
        }
        let mut queue = self.shared.lock();
        queue.release_decoder = true;
        queue.awaiting_keyframe = true;
        queue.retry_request = None;
        let dropped = queue.clear();
        drop(queue);
        Counters::add(&self.shared.counters.frames_dropped, dropped);
        self.shared.wake.notify_one();
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
            Err(_) => {
                self.shared.lock().closed = true;
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

fn open_av1() -> Result<Box<dyn VideoDecoder>, CodecError> {
    Ok(Box::new(fastcord_video::av1::Decoder::open()?))
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
    }
}

struct Worker {
    shared: Arc<Shared>,
    open: OpenDecoder,
    sink: Option<Sink>,
    decoder: Option<Box<dyn VideoDecoder>>,
    /// The stream the decoder's references belong to.
    stream: Option<u32>,
    failed_keyframes: u32,
}

impl Worker {
    fn new(shared: Arc<Shared>, open: OpenDecoder, sink: Option<Sink>) -> Self {
        Self {
            shared,
            open,
            sink,
            decoder: None,
            stream: None,
            failed_keyframes: 0,
        }
    }

    fn run(mut self) {
        let _exit = CloseOnExit(Arc::clone(&self.shared));
        while let Some(frame) = self.next() {
            self.process(frame);
        }
        // The decoder and every picture it holds are freed here, on the thread
        // that opened them.
    }

    /// The next frame, waiting for one; `None` once the session ended.
    fn next(&mut self) -> Option<ReceivedVideo> {
        loop {
            let mut queue = self.shared.lock();
            if queue.closed {
                return None;
            }
            if queue.release_decoder {
                queue.release_decoder = false;
                drop(queue);
                self.decoder = None;
                self.stream = None;
                continue;
            }
            if let Some(frame) = queue.pop() {
                return Some(frame);
            }
            self.shared.wake.wait(&mut queue);
        }
    }

    fn process(&mut self, frame: ReceivedVideo) {
        let shared = Arc::clone(&self.shared);
        let counters = &shared.counters;
        let (ssrc, keyframe) = (frame.ssrc, frame.keyframe);
        let mut broken = false;
        if self.stream != Some(ssrc) {
            // Another stream: the old one's references mean nothing to it.
            self.decoder = None;
            self.stream = Some(ssrc);
            self.failed_keyframes = 0;
            broken = !keyframe;
        }
        if !broken {
            broken = self.decode(&frame).is_err();
        }
        if broken {
            self.fail(ssrc, keyframe);
            counters.decode_errors.fetch_add(1, Ordering::Relaxed);
        } else if keyframe {
            self.failed_keyframes = 0;
        }
        // Last: everything above is visible to whoever sees this counter.
        counters.frames_processed.fetch_add(1, Ordering::Release);
    }

    fn decode(&mut self, frame: &ReceivedVideo) -> Result<(), CodecError> {
        let decoder = match &mut self.decoder {
            Some(decoder) => decoder,
            empty => {
                let decoder = (self.open)()?;
                *self.shared.backend.lock() = Some(decoder.backend().name.clone());
                empty.insert(decoder)
            }
        };
        let counters = &self.shared.counters;
        let sink = &mut self.sink;
        let ssrc = frame.ssrc;
        let timestamp =
            Duration::from_nanos(u64::from(frame.timestamp) * 1_000_000_000 / RTP_CLOCK_HZ);
        let mut pictures = 0;
        let result = decoder.decode(&frame.data, timestamp, &mut |picture| {
            pictures += 1;
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
        if let Err(CodecError::Platform { .. } | CodecError::Stalled(_)) = &result {
            // A native context that failed is not trusted again.
            self.decoder = None;
        }
        result
    }

    /// The chain is broken at a frame of `ssrc`: drop what cannot decode and ask
    /// for a keyframe, unless one is already queued (decoding restarts at it)
    /// or keyframes keep failing.
    fn fail(&mut self, ssrc: u32, was_keyframe: bool) {
        if was_keyframe {
            self.failed_keyframes += 1;
        }
        let mut queue = self.shared.lock();
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
            // A keyframe is next: the stream restarts there.
            queue.awaiting_keyframe = false;
        }
        drop(queue);
        Counters::add(&self.shared.counters.frames_dropped, dropped);
    }
}

#[cfg(test)]
mod tests;
