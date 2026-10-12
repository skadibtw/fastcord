use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use fastcord_model::Snowflake;
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

/// Every AV1 frame offered ended up dropped or processed, exactly once.
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
    let control = Arc::clone(control);
    StreamDecoder::with(Config {
        open: Box::new(move || {
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
        sink: None,
    })
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
        open: Box::new(open_av1),
        request_keyframe: requester(requests, 0),
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
fn nothing_starts_before_an_av1_keyframe_and_h264_is_counted_not_decoded() {
    let (control, requests, mut decoder) = setup();
    assert_eq!(decoder.stats(), DecodeStats::default());
    assert!(decoder.thread.is_none());
    for index in 0..3 {
        decoder.push(ReceivedVideo {
            codec: VideoCodec::H264,
            ..video(SSRC, index, index == 0, vec![1, 2, 3])
        });
    }
    // No thread, no decoder, and no request for a codec nobody decodes here.
    assert!(decoder.thread.is_none());
    assert_eq!(control.opens(), 0);
    assert!(requests.lock().is_empty());
    let stats = decoder.stats();
    assert_eq!((stats.frames_unsupported, stats.frames_received), (3, 0));
    // An AV1 delta frame cannot begin a stream either: it is dropped, and the
    // voice driver (whose receiver also awaits a keyframe) is the one to ask.
    let stats = push_settled(&mut decoder, video(SSRC, 3, false, vec![9]));
    assert_eq!(stats.frames_dropped, 1);
    assert!(decoder.thread.is_none());
    assert!(requests.lock().is_empty());
    // The first keyframe starts the thread, and the thread opens the decoder.
    push_settled(&mut decoder, video(SSRC, 4, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 5, false, vec![2]));
    assert!(decoder.thread.is_some());
    assert_eq!(control.opens(), 1);
    let stats = decoder.finish();
    assert_eq!(stats.backend.as_deref(), Some("scripted decoder"));
    assert_eq!(stats.pictures_decoded, 2);
    assert_eq!(control.log(), [1, 2]);
    assert_eq!(control.drops(), 1, "the decoder was freed with the thread");
    assert_accounted(&stats);
}

#[test]
fn leaving_av1_frees_the_decoder_and_a_later_keyframe_opens_a_new_one() {
    let (control, requests, mut decoder) = setup();
    push_settled(&mut decoder, video(SSRC, 0, true, vec![1]));
    push_settled(&mut decoder, video(SSRC, 1, false, vec![2]));
    // The sender switched codec: what the decoder holds describes nothing that
    // will arrive.
    decoder.push(ReceivedVideo {
        codec: VideoCodec::H264,
        ..video(SSRC, 2, true, vec![3])
    });
    control.wait_drops(1);
    // Nothing of the old stream continues into the new one.
    let stats = push_settled(&mut decoder, video(SSRC, 3, false, vec![4]));
    assert_eq!(stats.frames_dropped, 1);
    push_settled(&mut decoder, video(SSRC, 4, true, vec![5]));
    assert_eq!(control.opens(), 2);
    let stats = decoder.finish();
    assert_eq!(control.log(), [1, 2, 5]);
    assert_eq!(control.drops(), 2);
    assert!(requests.lock().is_empty());
    assert_eq!(stats.frames_unsupported, 1);
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
    assert_eq!(stats.decode_errors, 1);
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
    let stats = decoder.stats();
    assert_eq!(stats.frames_dropped, 19);
    assert!(requests.lock().is_empty());
    assert!(decoder.shared.lock().frames.is_empty());
    assert_eq!(control.opens(), 1);
    assert_eq!(control.log(), [PANIC]);
}
