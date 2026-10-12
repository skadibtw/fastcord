use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::ThreadId;
use std::time::Instant;

use fastcord_model::Snowflake;
use fastcord_video::frame::ColorDescription;
use fastcord_video::{Backend, BackendKind, Nv12};

use super::*;

const SSRC: u32 = 5_000;
const IVF: &[u8] = include_bytes!("../../../../../fixtures/video/av1-lossless-420-8.ivf");
const PIXELS: &[u8] = include_bytes!("../../../../../fixtures/video/av1-lossless-420-8.nv12");
const FRAME_BYTES: usize = 1449;

/// What the transport delivers for each of the fixture's three temporal units:
/// the sized OBUs, without the temporal delimiter RTP never carries.
fn units() -> Vec<Vec<u8>> {
    let header = usize::from(u16::from_le_bytes([IVF[6], IVF[7]]));
    let mut rest = &IVF[header..];
    let mut units = Vec::new();
    while !rest.is_empty() {
        let len = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        units.push(
            rest[12..12 + len]
                .strip_prefix(&[0x12, 0])
                .unwrap()
                .to_vec(),
        );
        rest = &rest[12 + len..];
    }
    assert_eq!(units.len(), 3);
    units
}

fn picture(index: usize) -> &'static [u8] {
    &PIXELS[index * FRAME_BYTES..(index + 1) * FRAME_BYTES]
}

fn video(ssrc: u32, index: u32, keyframe: bool, data: Vec<u8>) -> ReceivedVideo {
    ReceivedVideo {
        user_id: Snowflake(7),
        ssrc,
        codec: VideoCodec::Av1,
        timestamp: index * 3_000,
        keyframe,
        data,
    }
}

/// Polls until `done` holds (the decoder runs on its own thread).
fn wait_for(decoder: &StreamDecoder, done: impl Fn(&DecodeStats) -> bool) -> DecodeStats {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let stats = decoder.stats();
        if done(&stats) {
            return stats;
        }
        assert!(
            Instant::now() < deadline,
            "the decoder never reached the awaited state: {stats:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Offers one frame and returns once it has been dropped or fully processed,
/// side effects (keyframe requests, queue changes) included.
fn push_settled(decoder: &mut StreamDecoder, frame: ReceivedVideo) -> DecodeStats {
    let before = decoder.stats();
    decoder.push(frame);
    let pushed = decoder.stats();
    if pushed.frames_dropped > before.frames_dropped {
        return pushed;
    }
    wait_for(decoder, |stats| {
        stats.frames_processed > before.frames_processed
    })
}

/// Every encoded frame offered ended up dropped or processed, exactly once.
fn assert_accounted(stats: &DecodeStats) {
    assert_eq!(
        stats.frames_received,
        stats.frames_dropped + stats.frames_processed,
        "{stats:?}"
    );
}

/// Keyframe requests as the voice session would see them; the first `full`
/// attempts find its control queue full.
fn requester(log: &Arc<Mutex<Vec<u32>>>, full: usize) -> RequestKeyframe {
    let log = Arc::clone(log);
    let refused = AtomicUsize::new(full);
    Arc::new(move |ssrc| {
        if refused
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(MediaSendError::Full);
        }
        log.lock().push(ssrc);
        Ok(())
    })
}

// ---- a scripted decoder: the first byte of a frame says what happens --------

const BROKEN: u8 = 0xEE;
const UNSUPPORTED: u8 = 0xED;
const PLATFORM: u8 = 0xEC;
/// Waits until released, then decodes.
const BLOCK: u8 = 0xB0;
/// Waits until released, then fails.
const BLOCK_THEN_BROKEN: u8 = 0xB1;
const PANIC: u8 = 0xFF;

#[derive(Default)]
struct Control {
    /// First byte of every frame the decoder was given, in order.
    log: Mutex<Vec<u8>>,
    codecs: Mutex<Vec<VideoCodec>>,
    opened_on: Mutex<Vec<ThreadId>>,
    dropped_on: Mutex<Vec<ThreadId>>,
    statuses: Mutex<Vec<StreamStatus>>,
    opens: AtomicUsize,
    drops: AtomicUsize,
    entered: Mutex<bool>,
    released: Mutex<bool>,
    changed: Condvar,
}

impl Control {
    /// Waits until the decoder is inside a blocking frame.
    fn wait_entered(&self) {
        let mut entered = self.entered.lock();
        while !*entered {
            assert!(
                !self
                    .changed
                    .wait_for(&mut entered, Duration::from_secs(30))
                    .timed_out(),
                "the decoder never began the blocking frame"
            );
        }
    }

    fn release(&self) {
        *self.released.lock() = true;
        self.changed.notify_all();
    }

    fn log(&self) -> Vec<u8> {
        self.log.lock().clone()
    }

    fn opens(&self) -> usize {
        self.opens.load(Ordering::SeqCst)
    }

    fn drops(&self) -> usize {
        self.drops.load(Ordering::SeqCst)
    }

    fn assert_owner_thread(&self) {
        let opened = self.opened_on.lock();
        let dropped = self.dropped_on.lock();
        assert_eq!(*opened, *dropped, "every native context stays on its owner");
        assert!(opened.iter().all(|id| *id != std::thread::current().id()));
    }

    fn wait_drops(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.drops() < count {
            assert!(Instant::now() < deadline, "no decoder was freed");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

struct Scripted {
    control: Arc<Control>,
    backend: Backend,
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.control
            .dropped_on
            .lock()
            .push(std::thread::current().id());
        self.control.drops.fetch_add(1, Ordering::SeqCst);
    }
}

impl VideoDecoder for Scripted {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn decode(
        &mut self,
        unit: &[u8],
        timestamp: Duration,
        out: &mut dyn FnMut(DecodedFrame<'_>),
    ) -> Result<(), CodecError> {
        let id = unit[0];
        self.control.log.lock().push(id);
        match id {
            BROKEN => return Err(CodecError::InvalidBitstream("scripted")),
            UNSUPPORTED => return Err(CodecError::Unsupported("scripted")),
            PLATFORM => {
                return Err(CodecError::Platform {
                    operation: "scripted",
                    code: -1,
                });
            }
            PANIC => panic!("scripted decoder panic"),
            BLOCK | BLOCK_THEN_BROKEN => {
                *self.control.entered.lock() = true;
                self.control.changed.notify_all();
                let mut released = self.control.released.lock();
                while !*released {
                    self.control.changed.wait(&mut released);
                }
                drop(released);
                if id == BLOCK_THEN_BROKEN {
                    return Err(CodecError::InvalidBitstream("scripted"));
                }
            }
            _ => {}
        }
        out(DecodedFrame {
            image: Nv12::packed(2, 2, &[7; 6]).unwrap(),
            color: ColorDescription::default(),
            timestamp,
        });
        Ok(())
    }

    fn flush(&mut self, _: &mut dyn FnMut(DecodedFrame<'_>)) -> Result<(), CodecError> {
        Ok(())
    }
}

fn scripted_decoder(
    control: &Arc<Control>,
    requests: &Arc<Mutex<Vec<u32>>>,
    full: usize,
) -> StreamDecoder {
    StreamDecoder::with(scripted_config(control, requests, full))
}

fn scripted_config(control: &Arc<Control>, requests: &Arc<Mutex<Vec<u32>>>, full: usize) -> Config {
    let status_control = Arc::clone(control);
    let control = Arc::clone(control);
    Config {
        open: Box::new(move |codec| {
            control.codecs.lock().push(codec);
            control.opened_on.lock().push(std::thread::current().id());
            control.opens.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(Scripted {
                control: Arc::clone(&control),
                backend: Backend {
                    kind: BackendKind::Software,
                    api: "scripted",
                    name: "scripted decoder".into(),
                },
            }))
        }),
        request_keyframe: requester(requests, full),
        visible: Arc::new(AtomicBool::new(true)),
        sink: None,
        status: Arc::new(move |status| status_control.statuses.lock().push(status)),
    }
}

fn setup() -> (Arc<Control>, Arc<Mutex<Vec<u32>>>, StreamDecoder) {
    let control = Arc::new(Control::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let decoder = scripted_decoder(&control, &requests, 0);
    (control, requests, decoder)
}

// ---- the real decoder ---------------------------------------------------------

fn real_decoder(requests: &Arc<Mutex<Vec<u32>>>) -> (StreamDecoder, mpsc::Receiver<Picture>) {
    let (pictures, received) = mpsc::channel();
    let decoder = StreamDecoder::with(Config {
        open: Box::new(open_decoder),
        request_keyframe: requester(requests, 0),
        visible: Arc::new(AtomicBool::new(true)),
        status: Arc::new(|_| {}),
        sink: Some(Box::new(move |ssrc, picture| {
            let mut packed = Vec::new();
            picture.image.copy_packed_into(&mut packed);
            pictures.send((ssrc, picture.timestamp, packed)).unwrap();
        })),
    });
    (decoder, received)
}

type Picture = (u32, Duration, Vec<u8>);

/// The genuine libaom stream, as the transport delivers it, through the
/// dedicated thread and dav1d: the pictures match the reference decoder's byte
/// for byte, in order, with the timestamps the RTP clock gave them.
#[test]
fn a_real_av1_stream_decodes_to_the_reference_pictures() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut decoder, received) = real_decoder(&requests);
    for (index, unit) in units().into_iter().enumerate() {
        decoder.push(video(SSRC, index as u32, index == 0, unit));
        let (ssrc, timestamp, packed) = received.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(ssrc, SSRC);
        // 90 kHz ticks, 3000 per frame: 1/30 s (floored to the nanosecond).
        assert_eq!(timestamp, Duration::from_nanos(index as u64 * 33_333_333));
        assert_eq!(packed, picture(index), "picture {index}");
    }
    let stats = decoder.finish();
    assert_eq!(
        stats,
        DecodeStats {
            frames_received: 3,
            frames_processed: 3,
            pictures_decoded: 3,
            width: 35,
            height: 27,
            backend: Some("dav1d 1.5.4 (bundled portable C)".into()),
            ..DecodeStats::default()
        }
    );
    assert!(requests.lock().is_empty());
}

/// Damage the real decoder detects is answered with exactly one request, the
/// dependent frames are dropped without being decoded, and the next keyframe
/// restores the exact pictures.
#[test]
fn a_real_decoder_error_asks_once_and_the_next_keyframe_recovers_exactly() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut decoder, received) = real_decoder(&requests);
    let units = units();
    push_settled(&mut decoder, video(SSRC, 0, true, units[0].clone()));
    assert_eq!(received.recv().unwrap().2, picture(0));
    // An OBU claiming more bytes than it has: refused before the native decoder.
    let stats = push_settled(&mut decoder, video(SSRC, 1, false, vec![0x32, 0x7F, 1, 2]));
    assert_eq!(stats.decode_errors, 1);
    assert_eq!(*requests.lock(), [SSRC]);
    // The chain is broken: the genuine inter frame is not decoded at all.
    let stats = push_settled(&mut decoder, video(SSRC, 2, false, units[2].clone()));
    assert_eq!((stats.decode_errors, stats.frames_dropped), (1, 1));
    assert_eq!(requests.lock().len(), 1);
    // A keyframe restarts it, and the inter frame after it decodes again.
    push_settled(&mut decoder, video(SSRC, 3, true, units[0].clone()));
    assert_eq!(received.recv().unwrap().2, picture(0));
    push_settled(&mut decoder, video(SSRC, 4, false, units[1].clone()));
    assert_eq!(received.recv().unwrap().2, picture(1));
    let stats = decoder.finish();
    assert_eq!((stats.pictures_decoded, stats.keyframes_requested), (3, 1));
    assert_accounted(&stats);
}

// ---- laziness ------------------------------------------------------------------

#[test]
fn nothing_starts_before_a_visible_keyframe() {
    let (control, requests, mut decoder) = setup();
    assert_eq!(decoder.stats(), DecodeStats::default());
    for index in 0..3 {
        push_settled(&mut decoder, video(SSRC, index, false, vec![9]));
    }
    assert_eq!(decoder.stats().frames_dropped, 3);
    assert!(decoder.thread.is_none());
    assert_eq!(control.opens(), 0);
    assert!(
        requests.lock().is_empty(),
        "transport requests the initial keyframe"
    );
    push_settled(&mut decoder, video(SSRC, 3, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 4, false, vec![2]));
    assert!(decoder.thread.is_some());
    assert_eq!(control.opens(), 1);
    let stats = decoder.finish();
    assert_eq!(stats.backend.as_deref(), Some("scripted decoder"));
    assert_eq!(stats.pictures_decoded, 2);
    assert_eq!(control.log(), [1, 2]);
    assert_eq!(control.drops(), 1, "the decoder was freed with the thread");
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn codec_switches_release_references_and_select_a_new_decoder_at_a_keyframe() {
    let (control, requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![2]));
    // The sender switched codec: what the decoder holds describes nothing that
    // will arrive.
    push_settled(
        &mut decoder,
        ReceivedVideo {
            codec: VideoCodec::H264,
            ..video(SSRC, 2, true, vec![3])
        },
    );
    control.wait_drops(1);
    // Nothing of the old stream continues into the new one.
    let stats = push_settled(&mut decoder, video(SSRC, 3, false, vec![4]));
    assert_eq!(stats.frames_dropped, 1);
    push_settled(&mut decoder, video(SSRC, 4, true, vec![5]));
    assert_eq!(control.opens(), 3);
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, 2, 3, 5]);
    assert_eq!(control.drops(), 3);
    assert_eq!(*requests.lock(), [SSRC]);
    assert_eq!(
        *control.codecs.lock(),
        [VideoCodec::Av1, VideoCodec::H264, VideoCodec::Av1]
    );
    control.assert_owner_thread();
    assert_eq!(stats.frames_unsupported, 0);
    assert_accounted(&stats);
}

// ---- the chain ---------------------------------------------------------------------

#[test]
fn a_broken_frame_drops_dependents_asks_once_and_a_keyframe_resumes() {
    let (control, requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![2]));
    let stats = push_settled(&mut decoder, video(SSRC, 2, false, vec![BROKEN]));
    assert_eq!(stats.decode_errors, 1);
    assert_eq!(*requests.lock(), [SSRC]);
    // Delta frames after it never reach the decoder, and nothing is asked
    // again while the stream waits.
    push_settled(&mut decoder, video(SSRC, 3, false, vec![4]));
    push_settled(&mut decoder, video(SSRC, 4, false, vec![5]));
    assert_eq!(requests.lock().len(), 1);
    push_settled(&mut decoder, video(SSRC, 5, true, vec![6]));
    push_settled(&mut decoder, video(SSRC, 6, false, vec![7]));
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, 2, BROKEN, 6, 7]);
    assert_eq!(
        (
            stats.pictures_decoded,
            stats.decode_errors,
            stats.frames_dropped,
            stats.keyframes_requested
        ),
        (4, 1, 2, 1)
    );
    // The same decoder served all of it.
    assert_eq!(control.opens(), 1);
    assert_accounted(&stats);
}

/// Frames queued behind a broken one are stale, up to a keyframe: that is the
/// restart point, so nothing is asked for.
#[test]
fn queued_frames_behind_a_failure_are_dropped_up_to_the_next_keyframe() {
    let (control, requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    decoder.push(video(SSRC, 1, false, vec![BLOCK_THEN_BROKEN]));
    control.wait_entered();
    // The decoder is inside the frame that will fail while these queue up.
    decoder.push(video(SSRC, 2, false, vec![3]));
    decoder.push(video(SSRC, 3, false, vec![4]));
    decoder.push(video(SSRC, 4, true, vec![5]));
    decoder.push(video(SSRC, 5, false, vec![6]));
    assert_eq!(decoder.shared.lock().frames.len(), MAX_QUEUED_FRAMES);
    control.release();
    wait_for(&decoder, |stats| stats.frames_processed == 4);
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, BLOCK_THEN_BROKEN, 5, 6]);
    assert_eq!(
        (
            stats.pictures_decoded,
            stats.decode_errors,
            stats.frames_dropped
        ),
        (3, 1, 2)
    );
    assert!(requests.lock().is_empty(), "a keyframe was already queued");
    assert_accounted(&stats);
}

#[test]
fn a_native_failure_distrusts_the_decoder_and_the_next_keyframe_opens_a_new_one() {
    let (control, requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![PLATFORM]));
    assert_eq!(*requests.lock(), [SSRC]);
    control.wait_drops(1);
    push_settled(&mut decoder, video(SSRC, 2, true, vec![3]));
    assert_eq!(control.opens(), 2);
    // A refusal that is only about the stream keeps the decoder.
    push_settled(&mut decoder, video(SSRC, 3, false, vec![BROKEN]));
    push_settled(&mut decoder, video(SSRC, 4, true, vec![5]));
    assert_eq!(control.opens(), 2);
    let stats = decoder.finish();
    assert_accounted(&stats);
}

#[test]
fn a_new_stream_gets_a_fresh_decoder_and_must_begin_at_a_keyframe() {
    let (control, requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![2]));
    // Another SSRC (the owner restarted its stream): the old references are
    // meaningless, so a new decoder takes its keyframe.
    push_settled(&mut decoder, video(SSRC + 2, 0, true, vec![3]));
    assert_eq!(control.opens(), 2);
    push_settled(&mut decoder, video(SSRC + 2, 1, false, vec![4]));
    // A delta frame of a third stream continues nothing: refused, and asked for.
    let stats = push_settled(&mut decoder, video(SSRC + 4, 0, false, vec![5]));
    assert_eq!((stats.decode_errors, stats.frames_dropped), (0, 1));
    assert_eq!(*requests.lock(), [SSRC + 4]);
    push_settled(&mut decoder, video(SSRC + 4, 1, true, vec![6]));
    assert_eq!(control.opens(), 3);
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, 2, 3, 4, 6]);
    assert_eq!(stats.pictures_decoded, 5);
    assert_eq!(control.drops(), 3, "every decoder was freed");
    assert_accounted(&stats);
}

#[test]
fn undecodable_keyframes_are_asked_for_at_most_three_times_in_a_row() {
    let (_control, requests, mut decoder) = setup();
    for index in 0..6 {
        push_settled(&mut decoder, video(SSRC, index, true, vec![UNSUPPORTED]));
    }
    // The first three failures are answered, then the stream is left alone:
    // its own keyframes are still tried, but none is demanded.
    assert_eq!(requests.lock().len(), 3);
    assert_eq!(decoder.stats().decode_errors, 6);
    // One good keyframe clears the count, so the next failure asks again.
    push_settled(&mut decoder, video(SSRC, 6, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 7, false, vec![BROKEN]));
    assert_eq!(requests.lock().len(), 4);
    assert_accounted(&decoder.finish());
}

#[test]
fn a_full_control_queue_is_retried_with_the_next_frame_not_forgotten() {
    let control = Arc::new(Control::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut decoder = scripted_decoder(&control, &requests, 1);
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![BROKEN]));
    assert!(
        requests.lock().is_empty(),
        "the first attempt found it full"
    );
    assert_eq!(decoder.stats().keyframes_requested, 0);
    // Any frame that arrives retries the request.
    push_settled(&mut decoder, video(SSRC, 2, false, vec![3]));
    assert_eq!(*requests.lock(), [SSRC]);
    assert_eq!(decoder.stats().keyframes_requested, 1);
    // Delivered: nothing further is asked.
    push_settled(&mut decoder, video(SSRC, 3, false, vec![4]));
    assert_eq!(requests.lock().len(), 1);
    // A keyframe cancels a request still waiting for the control queue.
    let mut decoder = scripted_decoder(&control, &requests, 1);
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![BROKEN]));
    push_settled(&mut decoder, video(SSRC, 2, true, vec![3]));
    push_settled(&mut decoder, video(SSRC, 3, false, vec![4]));
    assert_eq!(requests.lock().len(), 1, "no stale request was sent");
}

// ---- bounds ----------------------------------------------------------------------------

#[test]
fn a_decoder_that_cannot_keep_up_drops_the_chain_within_its_bounds() {
    let (control, requests, mut decoder) = setup();
    // The decoder takes the first frame and stalls inside it.
    decoder.push(video(SSRC, 0, true, vec![BLOCK]));
    control.wait_entered();
    for index in 1..=MAX_QUEUED_FRAMES as u32 {
        decoder.push(video(SSRC, index, false, vec![index as u8]));
    }
    assert_eq!(decoder.shared.lock().frames.len(), MAX_QUEUED_FRAMES);
    assert!(requests.lock().is_empty());
    // One more does not fit: the queued chain and this frame are dropped, the
    // stream waits for a keyframe, and the sender is asked once.
    decoder.push(video(SSRC, 9, false, vec![9]));
    {
        let queue = decoder.shared.lock();
        assert!(queue.frames.is_empty() && queue.bytes == 0 && queue.awaiting_keyframe);
    }
    assert_eq!(*requests.lock(), [SSRC]);
    decoder.push(video(SSRC, 10, false, vec![10]));
    decoder.push(video(SSRC, 11, false, vec![11]));
    assert_eq!(requests.lock().len(), 1, "waiting is not asked about again");
    assert_eq!(decoder.stats().frames_dropped, MAX_QUEUED_FRAMES as u64 + 3);
    // A keyframe is accepted again and decodes after the stalled frame.
    decoder.push(video(SSRC, 12, true, vec![12]));
    control.release();
    wait_for(&decoder, |stats| stats.frames_processed == 2);
    let stats = decoder.finish();
    assert_eq!(control.log(), [BLOCK, 12]);
    assert_eq!(stats.keyframes_requested, 1);
    assert_accounted(&stats);
}

#[test]
fn the_queue_is_bounded_in_bytes_too_and_a_keyframe_replaces_a_stale_queue() {
    let (control, requests, mut decoder) = setup();
    decoder.push(video(SSRC, 0, true, vec![BLOCK]));
    control.wait_entered();
    // Two of these exceed the byte cap together.
    let large = |index: u32, keyframe: bool| {
        video(
            SSRC,
            index,
            keyframe,
            vec![index as u8; MAX_QUEUED_BYTES / 2 + 1],
        )
    };
    decoder.push(large(1, false));
    assert_eq!(decoder.shared.lock().frames.len(), 1);
    decoder.push(large(2, false));
    {
        let queue = decoder.shared.lock();
        assert!(queue.frames.is_empty() && queue.bytes == 0 && queue.awaiting_keyframe);
    }
    assert_eq!(requests.lock().len(), 1);
    // A keyframe is accepted again; the next large frame queues behind it.
    decoder.push(video(SSRC, 3, true, vec![3]));
    decoder.push(large(4, false));
    assert_eq!(decoder.shared.lock().frames.len(), 2);
    // A large keyframe that does not fit supersedes everything stale and needs
    // no request: the stream restarts at it.
    decoder.push(large(5, true));
    {
        let queue = decoder.shared.lock();
        assert_eq!(queue.frames.len(), 1);
        assert!(queue.frames[0].keyframe && !queue.awaiting_keyframe);
        assert!(queue.bytes <= MAX_QUEUED_BYTES);
    }
    assert_eq!(requests.lock().len(), 1);
    control.release();
    wait_for(&decoder, |stats| stats.frames_processed == 2);
    let stats = decoder.finish();
    assert_eq!(control.log(), [BLOCK, 5]);
    assert_accounted(&stats);
}

// ---- teardown ---------------------------------------------------------------------------

#[test]
fn ending_the_session_frees_the_decoder_and_ignores_later_frames() {
    let (control, _requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    let shared = Arc::clone(&decoder.shared);
    drop(decoder);
    // Dropping ends the thread, which frees the decoder it opened.
    control.wait_drops(1);
    assert_eq!(control.opens(), 1);
    let queue = shared.lock();
    assert!(queue.closed && queue.frames.is_empty());
}

/// A frame still inside the decoder when the session ends: the thread finishes
/// it, discards what was queued, and then frees the decoder.
#[test]
fn ending_the_session_during_a_decode_discards_the_queue_and_frees_afterwards() {
    let (control, _requests, mut decoder) = setup();
    decoder.push(video(SSRC, 0, true, vec![BLOCK]));
    control.wait_entered();
    decoder.push(video(SSRC, 1, false, vec![2]));
    decoder.push(video(SSRC, 2, false, vec![3]));
    let shared = Arc::clone(&decoder.shared);
    drop(decoder);
    assert_eq!(control.drops(), 0, "the decoder is still busy");
    control.release();
    control.wait_drops(1);
    // The two queued frames never reached it.
    assert_eq!(control.log(), [BLOCK]);
    let counters = &shared.counters;
    assert_eq!(counters.frames_dropped.load(Ordering::SeqCst), 2);
}

#[test]
fn a_decoder_that_panics_closes_the_stream_instead_of_filling_its_queue() {
    let (control, requests, mut decoder) = setup();
    decoder.push(video(SSRC, 0, true, vec![PANIC]));
    wait_for(&decoder, |_| decoder.shared.lock().closed);
    // Everything after the failure is dropped quietly: no queue growth, and no
    // storm of keyframe requests for a decoder that is gone.
    for index in 1..20 {
        decoder.push(video(SSRC, index, index == 5, vec![index as u8]));
    }
    assert!(decoder.shared.lock().frames.is_empty());
    let stats = decoder.finish();
    assert_eq!(stats.frames_dropped, 19);
    assert!(requests.lock().is_empty());
    assert_eq!(
        control.statuses.lock().last(),
        Some(&StreamStatus::Failed(
            "Video decoding worker stopped unexpectedly".into()
        ))
    );
    assert_eq!(control.opens(), 1);
    assert_eq!(control.log(), [PANIC]);
    assert_eq!(control.drops(), 1);
    control.assert_owner_thread();
}

#[test]
fn h264_is_selected_lazily_on_the_decoding_thread() {
    let (control, requests, mut decoder) = setup();
    push_settled(
        &mut decoder,
        ReceivedVideo {
            codec: VideoCodec::H264,
            ..video(SSRC, 0, false, vec![9])
        },
    );
    assert!(decoder.thread.is_none());
    assert_eq!(control.opens(), 0);
    for index in 1..=2 {
        push_settled(
            &mut decoder,
            ReceivedVideo {
                codec: VideoCodec::H264,
                ..video(SSRC, index, index == 1, vec![index as u8])
            },
        );
    }
    let stats = decoder.finish();
    assert_eq!(*control.codecs.lock(), [VideoCodec::H264]);
    assert_eq!(control.log(), [1, 2]);
    assert_eq!(stats.pictures_decoded, 2);
    assert!(requests.lock().is_empty());
    control.assert_owner_thread();
    assert_accounted(&stats);
}

fn change_visibility(decoder: &StreamDecoder, visible: bool) {
    decoder.shared.visible.store(visible, Ordering::Release);
    decoder.visibility_changed();
}

#[tokio::test]
async fn pause_and_resume_release_references_without_waiting_for_media() {
    let (control, requests, mut decoder) = setup();
    let (visibility, mut changed) = tokio::sync::watch::channel(true);
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    decoder.shared.visible.store(false, Ordering::Release);
    visibility.send(false).unwrap();
    changed.changed().await.unwrap();
    changed.borrow_and_update();
    decoder.visibility_changed();
    control.wait_drops(1);
    assert!(decoder.shared.lock().suspended);
    assert_eq!(control.log(), [1], "suspension requires no incoming frame");
    assert!(requests.lock().is_empty());
    decoder.visibility_changed();
    for index in 1..=4 {
        push_settled(&mut decoder, video(SSRC, index, index == 2, vec![9]));
    }
    assert_eq!(control.opens(), 1);
    assert!(decoder.shared.lock().frames.is_empty());
    assert_eq!(
        control
            .statuses
            .lock()
            .iter()
            .filter(|status| **status == StreamStatus::Suspended)
            .count(),
        1
    );
    decoder.shared.visible.store(true, Ordering::Release);
    visibility.send(true).unwrap();
    changed.changed().await.unwrap();
    changed.borrow_and_update();
    decoder.visibility_changed();
    decoder.visibility_changed();
    assert_eq!(*requests.lock(), [SSRC], "one coalesced resume PLI");
    assert_eq!(
        control.opens(),
        1,
        "resume alone does not allocate a decoder"
    );
    push_settled(&mut decoder, video(SSRC, 5, false, vec![9]));
    push_settled(&mut decoder, video(SSRC, 6, true, vec![2]));
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, 2]);
    assert_eq!(control.opens(), 2);
    assert_eq!(control.drops(), 2);
    assert_eq!((stats.frames_dropped, stats.keyframes_requested), (5, 1));
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn an_initially_hidden_watch_is_lazy_and_resumes_from_the_announced_source() {
    let control = Arc::new(Control::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let config = scripted_config(&control, &requests, 0);
    config.visible.store(false, Ordering::Release);
    let mut decoder = StreamDecoder::with(config);
    decoder.source_changed(SSRC);
    for index in 0..3 {
        push_settled(&mut decoder, video(SSRC, index, true, vec![9]));
    }
    assert!(decoder.thread.is_none());
    assert_eq!(control.opens(), 0);
    assert!(
        requests.lock().is_empty(),
        "hidden keyframes cause no PLI storm"
    );
    change_visibility(&decoder, true);
    change_visibility(&decoder, true);
    assert_eq!(*requests.lock(), [SSRC]);
    push_settled(&mut decoder, video(SSRC, 3, false, vec![9]));
    push_settled(&mut decoder, video(SSRC, 4, true, vec![1]));
    let stats = decoder.finish();
    assert_eq!(control.log(), [1]);
    assert_eq!(stats.frames_dropped, 4);
    assert_eq!(stats.keyframes_requested, 1);
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn a_resume_before_any_source_waits_to_request_the_first_known_ssrc_once() {
    let (control, requests, mut decoder) = setup();
    change_visibility(&decoder, false);
    change_visibility(&decoder, true);
    assert!(requests.lock().is_empty());
    for index in 0..3 {
        push_settled(&mut decoder, video(SSRC, index, false, vec![9]));
    }
    assert_eq!(*requests.lock(), [SSRC]);
    assert!(decoder.thread.is_none());
    push_settled(&mut decoder, video(SSRC, 3, true, vec![1]));
    let stats = decoder.finish();
    assert_eq!(control.opens(), 1);
    assert_eq!(stats.keyframes_requested, 1);
    assert_accounted(&stats);
}

#[test]
fn a_resume_pli_retries_a_full_control_queue_without_repeated_requests() {
    let control = Arc::new(Control::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut decoder = scripted_decoder(&control, &requests, 1);
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    change_visibility(&decoder, false);
    control.wait_drops(1);
    change_visibility(&decoder, true);
    change_visibility(&decoder, true);
    assert!(requests.lock().is_empty());
    push_settled(&mut decoder, video(SSRC, 1, false, vec![9]));
    push_settled(&mut decoder, video(SSRC, 2, false, vec![9]));
    assert_eq!(*requests.lock(), [SSRC]);
    push_settled(&mut decoder, video(SSRC, 3, true, vec![2]));
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, 2]);
    assert_eq!(stats.keyframes_requested, 1);
    assert_accounted(&stats);
}

#[test]
fn minimize_during_decode_discards_queued_frames_and_inflight_output() {
    let control = Arc::new(Control::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (pictures, received) = mpsc::channel();
    let mut config = scripted_config(&control, &requests, 0);
    config.sink = Some(Box::new(move |ssrc, frame| {
        pictures
            .send((
                ssrc,
                frame.timestamp,
                frame.color,
                std::thread::current().id(),
            ))
            .unwrap();
    }));
    let mut decoder = StreamDecoder::with(config);
    decoder.push(video(SSRC, 0, true, vec![BLOCK]));
    control.wait_entered();
    decoder.push(video(SSRC, 1, false, vec![9]));
    decoder.push(video(SSRC, 2, false, vec![9]));
    change_visibility(&decoder, false);
    push_settled(&mut decoder, video(SSRC, 3, true, vec![9]));
    {
        let queue = decoder.shared.lock();
        assert!(queue.frames.is_empty() && queue.bytes == 0);
    }
    control.release();
    control.wait_drops(1);
    assert!(matches!(
        received.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    assert!(requests.lock().is_empty());
    assert_eq!(control.log(), [BLOCK]);
    assert_eq!(
        *control.statuses.lock(),
        [StreamStatus::Suspended],
        "an old frame may not replace the hidden status"
    );
    change_visibility(&decoder, true);
    push_settled(&mut decoder, video(SSRC, 4, false, vec![9]));
    push_settled(&mut decoder, video(SSRC, 5, true, vec![2]));
    let (ssrc, timestamp, color, thread) = received.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(ssrc, SSRC);
    assert_eq!(timestamp, Duration::from_nanos(166_666_666));
    assert_eq!(color, ColorDescription::default());
    assert_eq!(thread, control.opened_on.lock()[1]);
    assert_ne!(thread, std::thread::current().id());
    let stats = decoder.finish();
    assert_eq!(control.log(), [BLOCK, 2]);
    assert_eq!(stats.frames_dropped, 4);
    assert_eq!(*requests.lock(), [SSRC]);
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn an_inflight_old_codec_failure_cannot_break_a_replacement_keyframe() {
    let (control, requests, mut decoder) = setup();
    decoder.push(video(SSRC, 0, true, vec![BLOCK_THEN_BROKEN]));
    control.wait_entered();
    decoder.push(video(SSRC, 1, false, vec![9]));
    decoder.push(ReceivedVideo {
        codec: VideoCodec::H264,
        ..video(SSRC + 2, 0, true, vec![2])
    });
    control.release();
    wait_for(&decoder, |stats| stats.frames_processed == 2);
    let stats = decoder.finish();
    assert_eq!(control.log(), [BLOCK_THEN_BROKEN, 2]);
    assert_eq!(*control.codecs.lock(), [VideoCodec::Av1, VideoCodec::H264]);
    assert_eq!((stats.frames_dropped, stats.decode_errors), (1, 1));
    assert!(
        requests.lock().is_empty(),
        "the replacement already has a keyframe"
    );
    assert_eq!(
        *control.statuses.lock(),
        [StreamStatus::Decoding {
            backend: "scripted software: scripted decoder".into()
        }]
    );
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn opening_failures_reach_the_ui_and_keep_keyframe_failure_throttling() {
    let control = Arc::new(Control::default());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let mut config = scripted_config(&control, &requests, 0);
    let mut real_open = config.open;
    let failed_attempts = Arc::clone(&attempts);
    config.open = Box::new(move |codec| {
        if codec == VideoCodec::H264 {
            failed_attempts.lock().push(std::thread::current().id());
            Err(CodecError::Platform {
                operation: "native H.264 open",
                code: -1,
            })
        } else {
            real_open(codec)
        }
    });
    let mut decoder = StreamDecoder::with(config);
    for index in 0..6 {
        push_settled(
            &mut decoder,
            ReceivedVideo {
                codec: VideoCodec::H264,
                ..video(SSRC, index, true, vec![9])
            },
        );
    }
    assert_eq!(attempts.lock().len(), 6);
    assert!(
        attempts
            .lock()
            .iter()
            .all(|id| *id != std::thread::current().id())
    );
    assert_eq!(*requests.lock(), [SSRC; 3]);
    assert_eq!(decoder.stats().decode_errors, 6);
    assert!(control.statuses.lock().iter().all(|status| {
        *status
            == StreamStatus::Failed(
                "Could not open H264 decoder: native H.264 open failed (0xFFFFFFFF)".into(),
            )
    }));
    push_settled(&mut decoder, video(SSRC, 6, true, vec![1]));
    assert_eq!(
        control.statuses.lock().last(),
        Some(&StreamStatus::Decoding {
            backend: "scripted software: scripted decoder".into()
        })
    );
    push_settled(&mut decoder, video(SSRC, 7, false, vec![BROKEN]));
    assert_eq!(
        requests.lock().len(),
        4,
        "a codec switch resets failure throttling"
    );
    let stats = decoder.finish();
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn decoding_failures_are_concrete_and_a_successful_keyframe_clears_the_error() {
    let (control, _requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![BROKEN]));
    assert_eq!(
        control.statuses.lock().last(),
        Some(&StreamStatus::Failed(
            "AV1 decode failed: invalid codec data: scripted".into()
        ))
    );
    push_settled(&mut decoder, video(SSRC, 2, true, vec![2]));
    assert_eq!(
        control.statuses.lock().last(),
        Some(&StreamStatus::Decoding {
            backend: "scripted software: scripted decoder".into()
        })
    );
    push_settled(&mut decoder, video(SSRC, 3, false, vec![PLATFORM]));
    assert_eq!(
        control.statuses.lock().last(),
        Some(&StreamStatus::Failed(
            "AV1 decode failed: scripted failed (0xFFFFFFFF)".into()
        ))
    );
    control.wait_drops(1);
    push_settled(&mut decoder, video(SSRC, 4, true, vec![3]));
    let stats = decoder.finish();
    assert_eq!(control.opens(), 2);
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn even_a_single_oversized_keyframe_cannot_exceed_the_encoded_byte_bound() {
    let (control, requests, mut decoder) = setup();
    decoder.push(video(SSRC, 0, true, vec![BLOCK]));
    control.wait_entered();
    decoder.push(video(SSRC, 1, false, vec![9]));
    for index in 2..5 {
        decoder.push(video(SSRC, index, true, vec![9; MAX_QUEUED_BYTES + 1]));
        let queue = decoder.shared.lock();
        assert!(queue.frames.is_empty() && queue.bytes == 0);
        assert!(queue.awaiting_keyframe);
    }
    assert_eq!(
        *requests.lock(),
        [SSRC],
        "oversized keyframes cause no PLI storm"
    );
    decoder.push(video(SSRC, 5, true, vec![2]));
    control.release();
    wait_for(&decoder, |stats| stats.frames_processed == 2);
    let stats = decoder.finish();
    assert_eq!(control.log(), [BLOCK, 2]);
    assert_eq!(stats.frames_dropped, 4);
    assert_accounted(&stats);
}

#[cfg(not(any(windows, target_os = "macos")))]
#[test]
fn unsupported_platforms_do_not_open_a_fabricated_h264_decoder() {
    assert!(matches!(
        open_decoder(VideoCodec::H264),
        Err(CodecError::PlatformUnavailable(_))
    ));
}

#[cfg(any(windows, target_os = "macos"))]
#[test]
#[ignore = "requires an installed native H.264 decoder; run on the M27 fixture runner"]
fn the_native_h264_factory_delivers_real_fixture_pixels_to_the_borrowed_sink() {
    use fastcord_video::h264::{inspect, nal_unit_type, nal_units};

    let fixture = include_bytes!("../../../../../fixtures/video/ipcm-128x96.h264");
    let mut units = Vec::new();
    let mut unit = Vec::new();
    // This committed I_PCM fixture has exactly one slice per picture.
    for nal in nal_units(fixture) {
        unit.extend_from_slice(&[0, 0, 0, 1]);
        unit.extend_from_slice(nal);
        if matches!(nal_unit_type(nal), 1 | 5) {
            units.push(std::mem::take(&mut unit));
        }
    }
    assert!(unit.is_empty());
    assert_eq!(units.len(), 3);
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (mut decoder, received) = real_decoder(&requests);
    for (index, data) in units.into_iter().enumerate() {
        let keyframe = inspect(&data).unwrap().idr;
        push_settled(
            &mut decoder,
            ReceivedVideo {
                codec: VideoCodec::H264,
                ..video(SSRC, index as u32, keyframe, data)
            },
        );
    }
    let (ssrc, timestamp, packed) = received.recv_timeout(Duration::from_secs(30)).unwrap();
    assert_eq!(ssrc, SSRC);
    assert_eq!(timestamp, Duration::ZERO);
    assert_eq!(packed.len(), 128 * 96 * 3 / 2);
    assert_eq!(&packed[..2], [16, 18]);
    assert_eq!(&packed[128 * 96..128 * 96 + 2], [16, 16]);
    let stats = decoder.finish();
    assert_eq!((stats.width, stats.height), (128, 96));
    assert_eq!(stats.decode_errors, 0);
    let backend = stats.backend.unwrap();
    assert!(
        backend.contains("Media Foundation")
            || backend.contains("VideoToolbox")
            || backend.contains("H264")
            || backend.contains("H.264"),
        "{backend}"
    );
    assert!(requests.lock().is_empty());
}

#[tokio::test]
async fn a_coalesced_hide_show_notification_releases_references_and_requests_once() {
    let (control, requests, mut decoder) = setup();
    let (visibility, mut changed) = tokio::sync::watch::channel(true);
    let observed = *changed.borrow_and_update();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    decoder.shared.visible.store(false, Ordering::Release);
    visibility.send(false).unwrap();
    decoder.shared.visible.store(true, Ordering::Release);
    visibility.send(true).unwrap();
    changed.changed().await.unwrap();
    let announced = *changed.borrow_and_update();
    assert!(
        observed && announced,
        "watch coalesces the intermediate false"
    );
    decoder.visibility_notified(observed, announced);
    control.wait_drops(1);
    assert_eq!(*requests.lock(), [SSRC]);
    push_settled(&mut decoder, video(SSRC, 1, false, vec![9]));
    assert_eq!(control.log(), [1]);
    push_settled(&mut decoder, video(SSRC, 2, true, vec![2]));
    let stats = decoder.finish();
    assert_eq!(control.opens(), 2);
    assert_eq!(control.log(), [1, 2]);
    assert_eq!(stats.keyframes_requested, 1);
    control.assert_owner_thread();
    assert_accounted(&stats);
}

#[test]
fn a_deferred_resume_request_uses_a_source_announcement_without_waiting_for_media() {
    let (_control, requests, decoder) = setup();
    change_visibility(&decoder, false);
    change_visibility(&decoder, true);
    assert!(requests.lock().is_empty());
    decoder.source_changed(SSRC);
    decoder.source_changed(SSRC);
    assert_eq!(*requests.lock(), [SSRC]);
    assert!(decoder.thread.is_none());
    assert_eq!(decoder.finish().keyframes_requested, 1);
}
