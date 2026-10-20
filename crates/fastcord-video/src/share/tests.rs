//! The share pipeline against fake encoders and a fake transport: keyframe and
//! bitrate control, pacing of repeats, size changes, refused units, failures
//! and the exactly-once end. The capture itself is covered by the platform
//! crate's tests, and the real capture → encoder path by the ignored hardware
//! tests in `hardware`.

#[cfg(windows)]
mod hardware;

use std::time::{Duration, Instant};

use fastcord_platform::capture::convert::nv12_len;
use fastcord_platform::capture::{CaptureError, CaptureState, Flow, Tick, VideoSample};

use super::pipeline::Pipeline;
use super::*;
use crate::codec::{Backend, BackendKind, CodecError};
use crate::frame::Nv12;

struct Log {
    opened: Vec<(EncoderConfig, BackendPreference)>,
    encodes: Vec<(Duration, bool, (u32, u32))>,
    bitrates: Vec<u32>,
    /// Encodes that fail before one succeeds.
    failing_encodes: u32,
    /// Opens that fail.
    failing_opens: u32,
    hardware: bool,
    settable: bool,
    unit_bytes: usize,
}

impl Default for Log {
    fn default() -> Self {
        Self {
            opened: Vec::new(),
            encodes: Vec::new(),
            bitrates: Vec::new(),
            failing_encodes: 0,
            failing_opens: 0,
            hardware: false,
            settable: true,
            unit_bytes: 200,
        }
    }
}

struct FakeEncoder {
    config: EncoderConfig,
    backend: Backend,
    log: Arc<Mutex<Log>>,
    produced: u32,
}

impl VideoEncoder for FakeEncoder {
    fn backend(&self) -> &Backend {
        &self.backend
    }

    fn config(&self) -> &EncoderConfig {
        &self.config
    }

    fn encode(
        &mut self,
        frame: &Nv12<'_>,
        timestamp: Duration,
        keyframe: bool,
        out: &mut Vec<EncodedFrame>,
    ) -> Result<(), CodecError> {
        let mut log = self.log.lock();
        if log.failing_encodes > 0 {
            log.failing_encodes -= 1;
            return Err(CodecError::Platform {
                operation: "fake encode",
                code: -1,
            });
        }
        log.encodes
            .push((timestamp, keyframe, (frame.width(), frame.height())));
        let idr = keyframe || self.produced == 0;
        self.produced += 1;
        let mut data = vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }];
        data.resize(log.unit_bytes, 0xAB);
        out.push(EncodedFrame {
            data,
            timestamp,
            keyframe: idr,
        });
        Ok(())
    }

    fn flush(&mut self, _out: &mut Vec<EncodedFrame>) -> Result<(), CodecError> {
        Ok(())
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<(), CodecError> {
        let mut log = self.log.lock();
        if !log.settable {
            return Err(CodecError::Unsupported("fixed rate"));
        }
        log.bitrates.push(bitrate);
        self.config.bitrate = bitrate;
        Ok(())
    }
}

struct Rig {
    pipeline: Pipeline,
    control: ShareControl,
    sent: Arc<Mutex<Vec<EncodedFrame>>>,
    outcome: Arc<Mutex<SinkOutcome>>,
    log: Arc<Mutex<Log>>,
    events: Arc<Mutex<Vec<ShareEvent>>>,
    shared: Arc<Shared>,
    now: Instant,
}

impl Rig {
    fn new(config: ShareConfig) -> Self {
        Self::with_preview(config, None)
    }

    fn with_preview(config: ShareConfig, preview: Option<PreviewSink>) -> Self {
        let shared = Arc::new(Shared::new(&config));
        let log = Arc::new(Mutex::new(Log::default()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let open_log = Arc::clone(&log);
        let open: OpenEncoder = Box::new(move |config, preference| {
            let mut log = open_log.lock();
            log.opened.push((config, preference));
            if log.failing_opens > 0 {
                log.failing_opens -= 1;
                return Err(OpenError::NoBackend(preference));
            }
            let kind = if log.hardware && preference != BackendPreference::Software {
                BackendKind::Hardware
            } else {
                BackendKind::Software
            };
            Ok(OpenedEncoder {
                encoder: Box::new(FakeEncoder {
                    config,
                    backend: Backend {
                        kind,
                        api: "Fake",
                        name: "fake encoder".to_owned(),
                    },
                    log: Arc::clone(&open_log),
                    produced: 0,
                }),
                rejected: Vec::new(),
            })
        });
        let record = Arc::clone(&events);
        let observer: ShareObserver = Arc::new(move |event| record.lock().push(event));
        Self {
            pipeline: Pipeline::new(Arc::clone(&shared), config, open, observer, preview),
            control: ShareControl {
                shared: Arc::clone(&shared),
            },
            sent: Arc::new(Mutex::new(Vec::new())),
            outcome: Arc::new(Mutex::new(SinkOutcome::Sent)),
            log,
            events,
            shared,
            now: Instant::now(),
        }
    }

    /// Attaches the sink and reports H.264 negotiated, like the transport does.
    fn armed(config: ShareConfig) -> Self {
        let rig = Self::new(config);
        rig.attach();
        rig.control.set_negotiated(true);
        rig
    }

    fn attach(&self) {
        let sent = Arc::clone(&self.sent);
        let outcome = Arc::clone(&self.outcome);
        self.control.attach(Box::new(move |unit| {
            let outcome = *outcome.lock();
            if outcome == SinkOutcome::Sent {
                sent.lock().push(unit);
            }
            outcome
        }));
    }

    fn picture(&mut self, width: u32, height: u32, source_ms: u64) -> Flow {
        let stride = width as usize;
        let bytes = vec![0x80; nv12_len(stride, height as usize)];
        let sample = VideoSample::nv12(
            width,
            height,
            stride,
            &bytes,
            Duration::from_secs(5000) + Duration::from_millis(source_ms),
        )
        .unwrap();
        self.pipeline.picture(&sample, self.now)
    }

    fn advance(&mut self, millis: u64) {
        self.now += Duration::from_millis(millis);
    }

    fn sent_keyframes(&self) -> Vec<bool> {
        self.sent.lock().iter().map(|unit| unit.keyframe).collect()
    }

    fn events(&self) -> Vec<ShareEvent> {
        self.events.lock().clone()
    }
}

fn config() -> ShareConfig {
    ShareConfig::SCREEN_SHARE_720P30
}

#[test]
fn the_preview_sees_every_picture_even_while_nothing_is_sent() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&seen);
    let preview: PreviewSink = Box::new(move |sample| {
        record.lock().push((sample.width(), sample.height()));
    });
    let mut rig = Rig::with_preview(ShareConfig::CAMERA_720P30, Some(preview));
    // Not attached, not negotiated: the camera preview still shows the capture.
    rig.picture(640, 360, 0);
    assert!(rig.log.lock().opened.is_empty());
    rig.attach();
    rig.control.set_negotiated(true);
    rig.picture(640, 360, 33);
    // Paused by the server: nothing is encoded, the preview carries on.
    rig.control.set_active(false);
    rig.picture(640, 360, 66);
    assert_eq!(*seen.lock(), [(640, 360); 3]);
    assert_eq!(rig.sent.lock().len(), 1);
    assert_eq!(ShareConfig::CAMERA_720P30.bitrate, 1_500_000);
    ShareConfig::CAMERA_720P30.validate().unwrap();
}

#[test]
fn nothing_is_encoded_until_a_sink_is_attached_and_h264_is_negotiated() {
    let mut rig = Rig::new(config());
    assert_eq!(rig.picture(640, 360, 0), Flow::Continue);
    rig.attach();
    assert_eq!(rig.picture(640, 360, 33), Flow::Continue);
    assert!(
        rig.log.lock().opened.is_empty(),
        "attached but not negotiated"
    );
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Idle);
    rig.control.set_negotiated(true);
    assert_eq!(rig.picture(640, 360, 66), Flow::Continue);
    assert_eq!(rig.sent.lock().len(), 1);
    let stats = rig.control.stats();
    assert_eq!((stats.pictures, stats.idle_pictures, stats.sent), (3, 2, 1));
    // The server taking H.264 back stops the stream without ending the share.
    rig.control.set_negotiated(false);
    rig.picture(640, 360, 100);
    assert_eq!(rig.sent.lock().len(), 1);
    assert_eq!(rig.control.stats().idle_pictures, 3);
}

#[test]
fn the_first_picture_opens_the_encoder_at_its_size_and_leaves_as_a_keyframe_at_time_zero() {
    let mut rig = Rig::armed(config());
    assert_eq!(rig.picture(1152, 720, 1234), Flow::Continue);
    let log = rig.log.lock();
    let (opened, preference) = log.opened[0];
    assert_eq!((opened.width, opened.height), (1152, 720));
    assert_eq!(
        (opened.frame_rate, opened.bitrate, opened.keyframe_interval),
        (30, 2_500_000, 150)
    );
    assert_eq!(preference, BackendPreference::Auto);
    drop(log);
    let sent = rig.sent.lock();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].keyframe);
    assert_eq!(sent[0].timestamp, Duration::ZERO);
    drop(sent);
    assert_eq!(
        rig.events(),
        [ShareEvent::Encoder(EncoderReport {
            backend: "Fake software: fake encoder".to_owned(),
            width: 1152,
            height: 720,
            bitrate: 2_500_000,
            skipped: Vec::new(),
            fell_back: false,
        })]
    );
    let stats = rig.control.stats();
    assert_eq!(
        (stats.opened, stats.width, stats.height, stats.bitrate),
        (1, 1152, 720, 2_500_000)
    );
    assert_eq!((stats.keyframes, stats.bytes), (1, 200));
}

#[test]
fn timestamps_follow_the_source_and_never_repeat_even_when_its_clock_does() {
    let mut rig = Rig::armed(config());
    for source in [100, 133, 133, 120, 200] {
        rig.picture(640, 360, source);
    }
    let times: Vec<Duration> = rig.sent.lock().iter().map(|u| u.timestamp).collect();
    let ms = |t: &Duration| t.as_millis();
    assert_eq!(
        times.iter().map(ms).collect::<Vec<_>>(),
        [0, 33, 34, 35, 100]
    );
}

#[test]
fn a_keyframe_request_is_applied_to_the_next_picture_and_only_to_it() {
    let mut rig = Rig::armed(config());
    rig.picture(640, 360, 0);
    rig.picture(640, 360, 33);
    rig.control.request_keyframe();
    rig.picture(640, 360, 66);
    rig.picture(640, 360, 100);
    assert_eq!(rig.sent_keyframes(), [true, false, true, false]);
    let log = rig.log.lock();
    assert_eq!(
        log.encodes.iter().map(|e| e.1).collect::<Vec<_>>(),
        [false, false, true, false]
    );
}

#[test]
fn a_static_screen_is_asked_for_again_on_a_keyframe_request_and_after_the_keepalive() {
    let mut rig = Rig::armed(config());
    // Armed with no picture yet: the capture is asked for its newest one.
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Repeat);
    rig.picture(640, 360, 0);
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Idle);
    rig.advance(900);
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Idle);
    rig.advance(100);
    assert_eq!(
        rig.pipeline.tick_at(rig.now),
        Tick::Repeat,
        "after one second"
    );
    rig.picture(640, 360, 1000);
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Idle);
    rig.control.request_keyframe();
    assert_eq!(
        rig.pipeline.tick_at(rig.now),
        Tick::Repeat,
        "keyframe wanted"
    );
    rig.picture(640, 360, 1010);
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Idle);
    // Paused or unattached: nothing is repeated.
    rig.control.set_active(false);
    rig.advance(5000);
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Idle);
}

#[test]
fn bitrate_requests_reach_the_running_encoder_clamped_to_the_bounds() {
    let mut rig = Rig::armed(config());
    rig.picture(640, 360, 0);
    rig.control.set_bitrate(1_000_000);
    rig.picture(640, 360, 33);
    rig.control.set_bitrate(1_000);
    rig.picture(640, 360, 66);
    rig.control.set_bitrate(50_000_000);
    rig.picture(640, 360, 100);
    let log = rig.log.lock();
    // Below the codec floor, above the configured ceiling: both clamped.
    assert_eq!(log.bitrates, [1_000_000, MIN_BITRATE, 2_500_000]);
    assert_eq!(
        log.opened.len(),
        1,
        "no reopen where the encoder can change rate"
    );
    drop(log);
    assert_eq!(rig.control.stats().bitrate, 2_500_000);
    assert_eq!(rig.sent_keyframes(), [true, false, false, false]);
}

#[test]
fn an_encoder_that_cannot_change_rate_is_reopened_at_the_new_rate_with_a_keyframe() {
    let mut rig = Rig::armed(config());
    rig.log.lock().settable = false;
    rig.picture(640, 360, 0);
    rig.control.set_bitrate(800_000);
    rig.picture(640, 360, 33);
    rig.picture(640, 360, 66);
    let log = rig.log.lock();
    assert_eq!(log.opened.len(), 2);
    assert_eq!(log.opened[1].0.bitrate, 800_000);
    drop(log);
    // The new encoder starts with an IDR, then continues.
    assert_eq!(rig.sent_keyframes(), [true, true, false]);
    assert_eq!(rig.control.stats().opened, 2);
}

#[test]
fn a_size_change_reopens_the_encoder_and_the_stream_continues_at_a_keyframe() {
    let mut rig = Rig::armed(config());
    rig.picture(1280, 720, 0);
    rig.picture(1280, 720, 33);
    rig.picture(800, 600, 66);
    rig.picture(800, 600, 100);
    let log = rig.log.lock();
    let sizes: Vec<_> = log.opened.iter().map(|o| (o.0.width, o.0.height)).collect();
    assert_eq!(sizes, [(1280, 720), (800, 600)]);
    drop(log);
    assert_eq!(rig.sent_keyframes(), [true, false, true, false]);
    // The timeline continues across the reopen.
    let times: Vec<u128> = rig
        .sent
        .lock()
        .iter()
        .map(|u| u.timestamp.as_millis())
        .collect();
    assert_eq!(times, [0, 33, 66, 100]);
    assert_eq!(
        rig.events()
            .iter()
            .filter(|event| matches!(event, ShareEvent::Encoder(_)))
            .count(),
        2
    );
}

#[test]
fn a_refused_unit_forces_one_keyframe_but_never_a_flood_of_them() {
    let mut rig = Rig::armed(config());
    rig.picture(640, 360, 0);
    *rig.outcome.lock() = SinkOutcome::Dropped;
    rig.picture(640, 360, 33);
    // The refusal asked for a keyframe; the next picture is one.
    *rig.outcome.lock() = SinkOutcome::Sent;
    rig.advance(10);
    rig.picture(640, 360, 66);
    assert_eq!(rig.sent_keyframes(), [true, true]);
    // Another refusal within the spacing does not ask again.
    *rig.outcome.lock() = SinkOutcome::Dropped;
    rig.advance(10);
    rig.picture(640, 360, 100);
    *rig.outcome.lock() = SinkOutcome::Sent;
    rig.advance(10);
    rig.picture(640, 360, 133);
    assert_eq!(rig.sent_keyframes(), [true, true, false]);
    // After the spacing it does.
    *rig.outcome.lock() = SinkOutcome::Dropped;
    rig.advance(400);
    rig.picture(640, 360, 166);
    *rig.outcome.lock() = SinkOutcome::Sent;
    rig.picture(640, 360, 200);
    assert_eq!(rig.sent_keyframes(), [true, true, false, true]);
    assert_eq!(rig.control.stats().dropped, 3);
}

#[test]
fn oversized_units_are_dropped_before_the_transport_sees_them() {
    let mut rig = Rig::armed(config());
    rig.log.lock().unit_bytes = MAX_ENCODED_FRAME_BYTES + 1;
    rig.picture(640, 360, 0);
    assert!(rig.sent.lock().is_empty());
    assert_eq!(rig.control.stats().dropped, 1);
    rig.log.lock().unit_bytes = 400;
    rig.advance(400);
    rig.picture(640, 360, 33);
    assert_eq!(
        rig.sent_keyframes(),
        [true],
        "the stream restarts at a keyframe"
    );
}

#[test]
fn a_closed_transport_ends_the_share_with_its_cause() {
    let mut rig = Rig::armed(config());
    rig.picture(640, 360, 0);
    *rig.outcome.lock() = SinkOutcome::Closed;
    assert_eq!(rig.picture(640, 360, 33), Flow::Stop);
    assert_eq!(
        rig.shared.failure.lock().clone(),
        Some(ShareEnd::SinkClosed)
    );
    // Once ended, nothing more happens.
    *rig.outcome.lock() = SinkOutcome::Sent;
    assert_eq!(rig.picture(640, 360, 66), Flow::Stop);
    assert_eq!(rig.pipeline.tick_at(rig.now), Tick::Stop);
    assert_eq!(rig.sent.lock().len(), 1);
}

#[test]
fn a_hardware_encoder_that_fails_while_running_is_replaced_by_software() {
    let mut rig = Rig::armed(config());
    rig.log.lock().hardware = true;
    rig.picture(640, 360, 0);
    rig.log.lock().failing_encodes = 1;
    // The failing picture is lost; the stream restarts at a keyframe.
    assert_eq!(rig.picture(640, 360, 33), Flow::Continue);
    rig.picture(640, 360, 66);
    rig.picture(640, 360, 100);
    let log = rig.log.lock();
    assert_eq!(log.opened.len(), 2);
    assert_eq!(log.opened[1].1, BackendPreference::Software);
    drop(log);
    assert_eq!(rig.sent_keyframes(), [true, true, false]);
    let events = rig.events();
    let ShareEvent::Encoder(report) = &events[1] else {
        panic!("{events:?}");
    };
    assert!(report.fell_back);
    assert!(report.backend.contains("software"), "{}", report.backend);
}

#[test]
fn an_explicit_hardware_choice_never_falls_back_and_repeated_failures_end_the_share() {
    let mut hardware = config();
    hardware.preference = BackendPreference::Hardware;
    let mut rig = Rig::armed(hardware);
    rig.log.lock().hardware = true;
    rig.picture(640, 360, 0);
    rig.log.lock().failing_encodes = 10;
    for step in 1..=3u64 {
        assert_eq!(rig.picture(640, 360, 33 * step), Flow::Continue);
    }
    assert!(
        rig.log
            .lock()
            .opened
            .iter()
            .all(|(_, preference)| *preference == BackendPreference::Hardware)
    );
    // The fourth failure in a row is final.
    assert_eq!(rig.picture(640, 360, 200), Flow::Stop);
    assert!(matches!(
        rig.shared.failure.lock().clone(),
        Some(ShareEnd::Encoder(detail)) if detail.contains("fake encode")
    ));
}

#[test]
fn an_encoder_that_cannot_open_ends_the_share_with_the_reason() {
    let mut rig = Rig::armed(config());
    rig.log.lock().failing_opens = 1;
    assert_eq!(rig.picture(640, 360, 0), Flow::Stop);
    assert_eq!(
        rig.shared.failure.lock().clone(),
        Some(ShareEnd::Encoder("no video codec is available".to_owned()))
    );
    assert!(rig.sent.lock().is_empty());
}

#[test]
fn pausing_stops_encoding_and_resuming_restarts_the_stream_at_a_keyframe() {
    let mut rig = Rig::armed(config());
    rig.picture(640, 360, 0);
    rig.picture(640, 360, 33);
    rig.control.set_active(false);
    rig.picture(640, 360, 66);
    rig.picture(640, 360, 100);
    assert_eq!(rig.sent.lock().len(), 2, "nothing is sent while paused");
    assert_eq!(rig.control.stats().idle_pictures, 2);
    rig.control.set_active(true);
    rig.picture(640, 360, 133);
    rig.picture(640, 360, 166);
    assert_eq!(rig.sent_keyframes(), [true, false, true, false]);
}

#[test]
fn a_late_transport_gets_its_stream_from_a_keyframe_even_after_deltas_were_encoded() {
    let mut rig = Rig::armed(config());
    rig.picture(640, 360, 0);
    rig.picture(640, 360, 33);
    // The connection restarted: a new sink replaces the old one.
    rig.sent.lock().clear();
    rig.attach();
    rig.picture(640, 360, 66);
    assert_eq!(rig.sent_keyframes(), [true]);
}

#[test]
fn the_share_ends_exactly_once_with_the_pipelines_cause_in_preference_to_a_plain_stop() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&events);
    let observer: ShareObserver = Arc::new(move |event| record.lock().push(event));
    let shared = Arc::new(Shared::new(&config()));
    let state = state_observer(Arc::clone(&shared), observer);
    state(&CaptureState::Starting);
    state(&CaptureState::Capturing);
    state(&CaptureState::Converting(ConversionPath::Gpu));
    state(&CaptureState::SourceSize {
        width: 800,
        height: 600,
    });
    shared.fail(ShareEnd::SinkClosed);
    shared.fail(ShareEnd::Encoder("later".to_owned()));
    state(&CaptureState::Stopped);
    state(&CaptureState::Failed(CaptureError::SourceClosed));
    assert_eq!(
        *events.lock(),
        [
            ShareEvent::Capturing,
            ShareEvent::Converting(ConversionPath::Gpu),
            ShareEvent::SourceSize {
                width: 800,
                height: 600
            },
            ShareEvent::Ended(ShareEnd::SinkClosed),
        ]
    );

    // A capture failure is reported as such; a plain stop as a stop.
    let events = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&events);
    let observer: ShareObserver = Arc::new(move |event| record.lock().push(event));
    let state = state_observer(Arc::new(Shared::new(&config())), observer);
    state(&CaptureState::Failed(CaptureError::PermissionDenied));
    assert_eq!(
        *events.lock(),
        [ShareEvent::Ended(ShareEnd::Capture(
            CaptureError::PermissionDenied
        ))]
    );
    let events = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&events);
    let observer: ShareObserver = Arc::new(move |event| record.lock().push(event));
    let state = state_observer(Arc::new(Shared::new(&config())), observer);
    state(&CaptureState::Stopped);
    assert_eq!(*events.lock(), [ShareEvent::Ended(ShareEnd::Stopped)]);
}

#[test]
fn configurations_are_validated_before_anything_starts() {
    config().validate().unwrap();
    for broken in [
        ShareConfig {
            bitrate: 1_000,
            ..config()
        },
        ShareConfig {
            max_bitrate: 1_000_000,
            ..config()
        },
        ShareConfig {
            idle_repeat: Duration::from_millis(5),
            ..config()
        },
        ShareConfig {
            keyframe_interval: 0,
            ..config()
        },
        ShareConfig {
            capture: CaptureConfig {
                width: 1920,
                height: 1080,
                frame_rate: 30,
            },
            ..config()
        },
    ] {
        assert!(broken.validate().is_err(), "{broken:?}");
    }
}

#[test]
fn controls_are_safe_before_a_capture_exists_and_after_it_ended() {
    let rig = Rig::new(config());
    // No capture session yet: pacing and waking have nobody to act on.
    rig.control.set_active(false);
    rig.control.set_active(true);
    rig.control.request_keyframe();
    rig.control.set_bitrate(500_000);
    assert!(rig.shared.keyframe.load(Ordering::Acquire));
    assert_eq!(rig.control.stats(), ShareStats::default());
}
