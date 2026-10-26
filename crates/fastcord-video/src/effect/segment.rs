//! Person segmentation on a dedicated worker thread (issue #85, ADR 0010).
//!
//! The capture thread never runs the model. It hands the worker at most one
//! reduced 256×256 RGB picture at a time (the next is offered only after the
//! worker took the last, so there is no queue), and reads back the newest
//! mask: a [`MASK_SIZE`]² array of 0..=255 person probabilities, temporally
//! smoothed, that the compositor stretches over every full-rate picture.
//!
//! The model is MediaPipe's selfie segmenter (Apache-2.0), translated once to
//! ONNX and run by `tract`, the runtime DeepFilterNet already brings. It is
//! parsed and optimized inside the worker, so a slow load never stalls the
//! camera, and everything is dropped when the worker is dropped.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use tract_onnx::prelude::*;

use super::MASK_SIZE;
use super::compose::Planes;

/// The pinned model (see `native/selfie/selfie-model.json`).
const MODEL: &[u8] = include_bytes!("../../native/selfie/selfie_segmenter.onnx");
const PIXELS: usize = MASK_SIZE * MASK_SIZE;

/// How much of the previous mask survives each new one (out of 256): damps
/// flicker at the cost of about one inference of lag.
const KEEP_PREVIOUS: f32 = 0.4;
/// Raw probabilities below `LOW` are background and above `HIGH` person; the
/// band between is a smooth edge.
const LOW: f32 = 0.3;
const HIGH: f32 = 0.7;

/// What the worker shares with the capture thread.
#[derive(Default)]
struct Inbox {
    /// A picture is waiting in `input`.
    ready: bool,
    stop: bool,
    /// Planar RGB, 3 × [`PIXELS`] floats in 0..=1.
    input: Vec<f32>,
}

#[derive(Default)]
struct Outbox {
    /// Counts finished masks; 0 means none yet.
    sequence: u64,
    mask: Vec<u8>,
    /// Why the worker ended on its own.
    failure: Option<String>,
}

struct Shared {
    inbox: Mutex<Inbox>,
    wake: Condvar,
    outbox: Mutex<Outbox>,
    /// The model is loaded and the worker is serving pictures.
    running: AtomicBool,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A running segmentation worker. Dropping it tells the thread to stop and
/// returns at once; the thread frees the model itself once its load or
/// current inference ends, never holding up the capture thread (#132).
pub struct Segmenter {
    shared: Arc<Shared>,
    /// The last mask sequence copied out.
    seen: u64,
}

impl Segmenter {
    pub fn spawn() -> Self {
        let shared = Arc::new(Shared {
            inbox: Mutex::new(Inbox {
                input: vec![0.0; 3 * PIXELS],
                ..Inbox::default()
            }),
            wake: Condvar::new(),
            outbox: Mutex::new(Outbox {
                mask: vec![0; PIXELS],
                ..Outbox::default()
            }),
            running: AtomicBool::new(false),
        });
        let worker = Arc::clone(&shared);
        let started = std::thread::Builder::new()
            .name("camera-segmentation".to_owned())
            .spawn(move || run(&worker));
        if started.is_err() {
            lock(&shared.outbox).failure = Some("the segmentation thread could not start".into());
        }
        Self { shared, seen: 0 }
    }

    /// Whether the model is loaded (tests and diagnostics).
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Acquire)
    }

    /// Why the worker ended on its own, if it did.
    pub fn failure(&self) -> Option<String> {
        lock(&self.shared.outbox).failure.clone()
    }

    /// Whether the worker has taken the last picture, so another may be
    /// offered.
    pub fn wants_picture(&self) -> bool {
        !lock(&self.shared.inbox).ready
    }

    /// Reduces `src` to the model's input and offers it. Returns `false` when
    /// the worker is still busy with the previous one.
    pub fn offer(&self, src: &Planes<'_>) -> bool {
        let mut inbox = lock(&self.shared.inbox);
        if inbox.ready || inbox.stop {
            return false;
        }
        reduce_to_input(src, &mut inbox.input);
        inbox.ready = true;
        drop(inbox);
        self.shared.wake.notify_one();
        true
    }

    /// Copies the newest mask into `mask` if it is newer than the last one
    /// copied; returns whether it changed.
    pub fn take_mask(&mut self, mask: &mut [u8]) -> bool {
        let outbox = lock(&self.shared.outbox);
        if outbox.sequence == self.seen {
            return false;
        }
        mask.copy_from_slice(&outbox.mask);
        self.seen = outbox.sequence;
        true
    }

    /// Blocks until the next mask is ready (tests).
    #[cfg(test)]
    pub fn wait_mask(&mut self, mask: &mut [u8]) {
        for _ in 0..4000 {
            if self.take_mask(mask) {
                return;
            }
            assert!(self.failure().is_none(), "{:?}", self.failure());
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("no mask within 20 s");
    }
}

impl Drop for Segmenter {
    fn drop(&mut self) {
        // Not joined: loading the model can take seconds and cannot be cut
        // short, and this runs on the capture thread.
        lock(&self.shared.inbox).stop = true;
        self.shared.wake.notify_one();
    }
}

pub(super) type Model = SimplePlan<TypedFact, Box<dyn TypedOp>, Graph<TypedFact, Box<dyn TypedOp>>>;

pub(super) fn load() -> TractResult<Model> {
    tract_onnx::onnx()
        .model_for_read(&mut &MODEL[..])?
        .with_input_fact(0, f32::fact([1, 3, MASK_SIZE, MASK_SIZE]).into())?
        .into_optimized()?
        .into_runnable()
}

fn run(shared: &Shared) {
    let model = match load() {
        Ok(model) => model,
        Err(error) => {
            lock(&shared.outbox).failure =
                Some(format!("the background model failed to load: {error}"));
            return;
        }
    };
    shared.running.store(true, Ordering::Release);
    let mut input = vec![0.0f32; 3 * PIXELS];
    let mut smoothed = vec![0.0f32; PIXELS];
    let mut first = true;
    loop {
        {
            let mut inbox = lock(&shared.inbox);
            while !inbox.ready && !inbox.stop {
                inbox = shared
                    .wake
                    .wait(inbox)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if inbox.stop {
                break;
            }
            std::mem::swap(&mut inbox.input, &mut input);
            inbox.ready = false;
        }
        match infer(&model, &input) {
            Ok(raw) => {
                smooth(&raw, &mut smoothed, first);
                first = false;
                let mut outbox = lock(&shared.outbox);
                for (byte, &value) in outbox.mask.iter_mut().zip(&smoothed) {
                    *byte = (value * 255.0 + 0.5) as u8;
                }
                outbox.sequence += 1;
            }
            Err(error) => {
                lock(&shared.outbox).failure =
                    Some(format!("the background model failed: {error}"));
                break;
            }
        }
    }
    shared.running.store(false, Ordering::Release);
}

pub(super) fn infer(model: &Model, input: &[f32]) -> TractResult<Vec<f32>> {
    let tensor = Tensor::from_shape(&[1, 3, MASK_SIZE, MASK_SIZE], input)?;
    let outputs = model.run(tvec!(tensor.into()))?;
    let view = outputs[0].to_array_view::<f32>()?;
    let raw: Vec<f32> = view.iter().copied().collect();
    if raw.len() != PIXELS {
        tract_onnx::tract_core::anyhow::bail!("unexpected output size {}", raw.len());
    }
    Ok(raw)
}

/// Sharpens the probabilities into a soft-edged matte and blends them with the
/// previous matte.
fn smooth(raw: &[f32], smoothed: &mut [f32], first: bool) {
    for (out, &p) in smoothed.iter_mut().zip(raw) {
        let t = ((p - LOW) / (HIGH - LOW)).clamp(0.0, 1.0);
        let matte = t * t * (3.0 - 2.0 * t);
        *out = if first {
            matte
        } else {
            *out * KEEP_PREVIOUS + matte * (1.0 - KEEP_PREVIOUS)
        };
    }
}

/// BT.709 limited-range NV12 reduced to planar RGB at the model size: each
/// input pixel averages four luma samples spread over its block, with the
/// chroma at the block center.
pub(super) fn reduce_to_input(src: &Planes<'_>, out: &mut [f32]) {
    let (w, h) = (src.width, src.height);
    for j in 0..MASK_SIZE {
        let y_lo = ((j * 2 + 1) * h / (MASK_SIZE * 4)).min(h - 1);
        let y_hi = ((j * 2 + 3) * h / (MASK_SIZE * 4)).min(h - 1);
        let y_mid = ((j * 2 + 1) * h / (MASK_SIZE * 2)).min(h - 1);
        for i in 0..MASK_SIZE {
            let x_lo = ((i * 2 + 1) * w / (MASK_SIZE * 4)).min(w - 1);
            let x_hi = ((i * 2 + 3) * w / (MASK_SIZE * 4)).min(w - 1);
            let x_mid = ((i * 2 + 1) * w / (MASK_SIZE * 2)).min(w - 1);
            let luma = |x: usize, y: usize| f32::from(src.y[y * src.y_stride + x]);
            let y =
                (luma(x_lo, y_lo) + luma(x_hi, y_lo) + luma(x_lo, y_hi) + luma(x_hi, y_hi)) / 4.0;
            let chroma = (y_mid / 2) * src.uv_stride + (x_mid / 2) * 2;
            let u = f32::from(src.uv[chroma]) - 128.0;
            let v = f32::from(src.uv[chroma + 1]) - 128.0;
            let y = 1.164 * (y - 16.0);
            let index = j * MASK_SIZE + i;
            out[index] = ((y + 1.793 * v) / 255.0).clamp(0.0, 1.0);
            out[PIXELS + index] = ((y - 0.213 * u - 0.533 * v) / 255.0).clamp(0.0, 1.0);
            out[2 * PIXELS + index] = ((y + 2.112 * u) / 255.0).clamp(0.0, 1.0);
        }
    }
}

/// Runs the model once, synchronously, on a planar RGB picture (tests and
/// the leakage fixture).
#[cfg(test)]
pub fn infer_once(input: &[f32]) -> Vec<f32> {
    infer(&load().expect("model loads"), input).expect("model runs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::fixture::{scene_rgb, scene_truth};

    #[test]
    fn the_pinned_model_files_match_their_recorded_hashes() {
        use sha2::{Digest, Sha256};
        let json: serde_json::Value =
            serde_json::from_str(include_str!("../../native/selfie/selfie-model.json")).unwrap();
        let hex = |bytes: &[u8]| {
            Sha256::digest(bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        };
        assert_eq!(hex(MODEL), json["onnx_sha256"].as_str().unwrap());
        let tflite = include_bytes!("../../native/selfie/selfie_segmenter.tflite");
        assert_eq!(hex(tflite), json["source_sha256"].as_str().unwrap());
    }

    /// The same synthetic scene run through TensorFlow Lite (ai-edge-litert
    /// 1.x on the original `.tflite`) gave these values; the ONNX translation
    /// run by tract must agree.
    #[test]
    fn tract_agrees_with_the_tensorflow_lite_reference() {
        let rgb = scene_rgb(MASK_SIZE, MASK_SIZE);
        let mask = infer_once(&rgb);
        let at = |i: usize, j: usize| mask[j * MASK_SIZE + i];
        for ((i, j), expected) in [
            ((128, 90), 1.0),
            ((128, 200), 1.0),
            ((20, 20), 0.0),
            ((240, 100), 0.0),
            ((100, 135), 0.011_788_914), // an edge pixel: looser
            ((60, 200), 1.0),
        ] {
            assert!(
                (at(i, j) - expected).abs() < 1e-2,
                "({i}, {j}) {}",
                at(i, j)
            );
        }
        let sum: f32 = mask.iter().sum();
        assert!((sum - 22_803.268).abs() < 25.0, "sum {sum}");
    }

    #[test]
    fn the_matte_recovers_a_rendered_person_with_little_leakage() {
        let truth = scene_truth(MASK_SIZE, MASK_SIZE);
        let mut smoothed = vec![0.0; PIXELS];
        smooth(
            &infer_once(&scene_rgb(MASK_SIZE, MASK_SIZE)),
            &mut smoothed,
            true,
        );
        let (mut both, mut either, mut leaked, mut background) = (0, 0, 0, 0);
        for (&p, &person) in smoothed.iter().zip(&truth) {
            let predicted = p > 0.5;
            both += usize::from(predicted && person);
            either += usize::from(predicted || person);
            if !person {
                background += 1;
                leaked += usize::from(p > 0.1);
            }
        }
        let iou = both as f32 / either as f32;
        let leakage = leaked as f32 / background as f32;
        assert!(iou > 0.95, "IoU {iou}");
        assert!(leakage < 0.03, "leakage {leakage}");
    }

    #[test]
    fn smoothing_damps_flicker_and_the_first_mask_is_taken_whole() {
        let mut smoothed = vec![0.0; 2];
        smooth(&[1.0, 0.0], &mut smoothed, true);
        assert_eq!(smoothed, [1.0, 0.0]);
        smooth(&[0.0, 1.0], &mut smoothed, false);
        assert!((smoothed[0] - KEEP_PREVIOUS).abs() < 1e-6);
        assert!((smoothed[1] - (1.0 - KEEP_PREVIOUS)).abs() < 1e-6);
        // Mid probabilities map to a soft edge, not a hard threshold.
        let mut edge = vec![0.0; 3];
        smooth(&[0.3, 0.5, 0.7], &mut edge, true);
        assert!(edge[0] < 0.01 && (edge[1] - 0.5).abs() < 0.01 && edge[2] > 0.99);
    }

    #[test]
    fn the_worker_loads_serves_a_mask_and_stops_when_dropped() {
        use crate::effect::fixture::scene_nv12;
        let (w, h) = (640usize, 360usize);
        let bytes = scene_nv12(w, h);
        let src = crate::effect::compose::Planes {
            width: w,
            height: h,
            y: &bytes[..w * h],
            y_stride: w,
            uv: &bytes[w * h..],
            uv_stride: w,
        };
        let mut segmenter = Segmenter::spawn();
        let shared = Arc::clone(&segmenter.shared);
        let mut mask = vec![0u8; PIXELS];
        assert!(segmenter.wants_picture());
        while !segmenter.offer(&src) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        segmenter.wait_mask(&mut mask);
        assert!(segmenter.is_running());
        assert!(mask.iter().any(|&m| m > 200) && mask.iter().any(|&m| m < 20));
        drop(segmenter);
        // The thread ends on its own and drops the model: nothing runs any more.
        for _ in 0..4000 {
            if Arc::strong_count(&shared) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!shared.running.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&shared), 1);
    }

    #[test]
    fn dropping_while_the_model_loads_does_not_wait_for_it() {
        let segmenter = Segmenter::spawn();
        let shared = Arc::clone(&segmenter.shared);
        let started = std::time::Instant::now();
        drop(segmenter);
        assert!(
            started.elapsed() < std::time::Duration::from_millis(50),
            "the capture thread waited {:?}",
            started.elapsed()
        );
        // The load finishes on the worker, which then sees `stop` and ends.
        for _ in 0..4000 {
            if Arc::strong_count(&shared) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(Arc::strong_count(&shared), 1);
        assert!(!shared.running.load(Ordering::Acquire));
    }
}
