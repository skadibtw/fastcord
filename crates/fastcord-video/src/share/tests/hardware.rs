//! The real path on this machine: a window with known pixels is captured by
//! the platform session, encoded by a Media Foundation transform, and the access
//! units are decoded again and compared with what was painted. Run by hand with
//! `cargo test -p fastcord-video --locked share::tests::hardware -- --ignored --nocapture --test-threads=1`.

use std::time::{Duration, Instant};

use fastcord_platform::capture::convert::{Bgra, Converter, nv12_len};
use fastcord_platform::capture::windows::test_window::{LEFT_RGB, RIGHT_RGB, TestWindow};

use super::*;
use crate::codec::{DecodedFrame, VideoDecoder};
use crate::h264::{NAL_SPS, inspect, nal_unit_type, nal_units, parse_sps};
use crate::media_foundation::open_decoder;

fn wait_for(deadline: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + deadline;
    while Instant::now() < end {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    done()
}

/// The NV12 triple the CPU converter assigns to a solid color.
fn expected_yuv([r, g, b]: [u8; 3]) -> [i32; 3] {
    let source: Vec<u8> = [b, g, r, 255].repeat(4);
    let image = Bgra::new(&source, 8, 2, 2).unwrap();
    let mut out = vec![0; nv12_len(2, 2)];
    Converter::default()
        .convert(&image, 2, 2, 2, &mut out)
        .unwrap();
    [i32::from(out[0]), i32::from(out[4]), i32::from(out[5])]
}

/// Mean Y, Cb, Cr of the 8x8 block around (`x`, `y`) of a packed NV12 picture.
fn mean_yuv(packed: &[u8], width: u32, height: u32, x: u32, y: u32) -> [i32; 3] {
    let luma_len = (width * height) as usize;
    let stride = width as usize;
    let (mut luma, mut cb, mut cr) = (0, 0, 0);
    for row in y - 4..y + 4 {
        for column in x - 4..x + 4 {
            luma += i32::from(packed[row as usize * stride + column as usize]);
        }
    }
    for row in (y - 4) / 2..(y + 4) / 2 {
        for pair in (x - 4) / 2..(x + 4) / 2 {
            let at = luma_len + row as usize * stride + pair as usize * 2;
            cb += i32::from(packed[at]);
            cr += i32::from(packed[at + 1]);
        }
    }
    [luma / 64, cb / 16, cr / 16]
}

fn assert_close(actual: [i32; 3], expected: [i32; 3], what: &str) {
    for (a, e) in actual.iter().zip(expected) {
        assert!(
            (a - e).abs() <= 12,
            "{what}: {actual:?} is not within 12 of {expected:?}"
        );
    }
}

fn share_a_window(preference: BackendPreference) {
    let window = TestWindow::open(640, 360, 120, 120);
    let frames = Arc::new(Mutex::new(Vec::<EncodedFrame>::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let record = Arc::clone(&events);
    let mut share = ShareConfig::SCREEN_SHARE_720P30;
    share.preference = preference;
    let worker = ShareWorker::start(
        window.id(),
        share,
        Arc::new(move |event| record.lock().push(event)),
    )
    .unwrap();
    let control = worker.control();
    // Nothing is encoded before the transport attaches and H.264 is negotiated.
    assert!(
        wait_for(Duration::from_secs(10), || control.stats().idle_pictures
            > 0),
        "pictures are captured but unused"
    );
    assert_eq!(control.stats().encoded, 0);
    let sink = Arc::clone(&frames);
    control.attach(Box::new(move |unit| {
        let mut frames = sink.lock();
        if frames.len() < 240 {
            frames.push(unit);
        }
        SinkOutcome::Sent
    }));
    control.set_negotiated(true);
    assert!(
        wait_for(Duration::from_secs(10), || frames.lock().len() >= 30),
        "{} units, events {:?}",
        frames.lock().len(),
        events.lock()
    );

    // The stream starts with a self-contained IDR picture of the window's size.
    let first = frames.lock()[0].clone();
    let summary = inspect(&first.data).unwrap();
    assert!(first.keyframe && summary.idr && summary.sps && summary.pps);
    let sps = nal_units(&first.data)
        .find(|nal| nal_unit_type(nal) == NAL_SPS)
        .unwrap();
    let sps = parse_sps(sps).unwrap();
    println!(
        "{preference:?}: {}x{} IDR of {} bytes",
        sps.width,
        sps.height,
        first.data.len()
    );
    assert!(sps.width <= 1280 && sps.height <= 720);
    assert!((640..800).contains(&sps.width) && (360..480).contains(&sps.height));
    let report = events
        .lock()
        .iter()
        .find_map(|event| match event {
            ShareEvent::Encoder(report) => Some(report.clone()),
            _ => None,
        })
        .expect("the encoder was reported");
    println!("encoder: {} (skipped {:?})", report.backend, report.skipped);
    let expected_kind = match preference {
        BackendPreference::Hardware => Some("hardware"),
        BackendPreference::Software => Some("software"),
        BackendPreference::Auto => None,
    };
    if let Some(kind) = expected_kind {
        assert!(report.backend.contains(kind), "{}", report.backend);
    }

    // A keyframe request is answered within a second even by a frozen screen.
    window.set_animated(false);
    std::thread::sleep(Duration::from_millis(300));
    let seen = frames.lock().len();
    control.request_keyframe();
    assert!(
        wait_for(Duration::from_secs(2), || frames.lock()[seen..]
            .iter()
            .any(|unit| unit.keyframe)),
        "no keyframe after the request"
    );
    // A new bitrate reaches the running encoder.
    control.set_bitrate(800_000);
    assert!(wait_for(Duration::from_secs(3), || control.stats().bitrate
        == 800_000));
    // A frozen screen keeps producing keepalive pictures about every second.
    let frozen = frames.lock().len();
    assert!(wait_for(Duration::from_secs(3), || frames.lock().len() > frozen));
    let stats = control.stats();
    println!("{stats:?}");
    assert_eq!(stats.dropped, 0);

    // Decoding everything reproduces the painted halves (limited-range BT.709).
    let units: Vec<EncodedFrame> = frames.lock().clone();
    let mut decoder = open_decoder(BackendPreference::Software).unwrap().codec;
    let mut last: Option<(u32, u32, Vec<u8>)> = None;
    let mut keep = |picture: DecodedFrame<'_>| {
        let mut packed = Vec::new();
        picture.image.copy_packed_into(&mut packed);
        last = Some((picture.image.width(), picture.image.height(), packed));
    };
    for unit in &units {
        decoder
            .decode(&unit.data, unit.timestamp, &mut keep)
            .unwrap();
    }
    decoder.flush(&mut keep).unwrap();
    let (width, height, packed) = last.expect("a picture was decoded");
    assert_eq!((width, height), (sps.width, sps.height));
    let (left, right, y) = (width / 4, width * 3 / 4, height * 6 / 10);
    assert_close(
        mean_yuv(&packed, width, height, left, y),
        expected_yuv(LEFT_RGB),
        "red half",
    );
    assert_close(
        mean_yuv(&packed, width, height, right, y),
        expected_yuv(RIGHT_RGB),
        "blue half",
    );

    // Stopping ends the capture thread and reports it once.
    worker.stop().unwrap();
    let ended: Vec<_> = events
        .lock()
        .iter()
        .filter(|event| matches!(event, ShareEvent::Ended(_)))
        .cloned()
        .collect();
    assert_eq!(ended, [ShareEvent::Ended(ShareEnd::Stopped)]);
}

#[test]
#[ignore = "captures a real window and encodes with Media Foundation; needs a Windows desktop session"]
fn a_shared_window_is_encoded_and_decoded_back_automatic_backend() {
    share_a_window(BackendPreference::Auto);
}

#[test]
#[ignore = "needs a GPU H.264 encoder and a Windows desktop session"]
fn a_shared_window_is_encoded_and_decoded_back_hardware_backend() {
    share_a_window(BackendPreference::Hardware);
}

#[test]
#[ignore = "captures a real window and encodes with Media Foundation; needs a Windows desktop session"]
fn a_shared_window_is_encoded_and_decoded_back_software_backend() {
    share_a_window(BackendPreference::Software);
}
