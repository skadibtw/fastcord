//! Bodies of the cpal data callbacks.
//!
//! They only convert samples and move them through a preallocated ring:
//! no allocation, no lock, no I/O (SPEC §2.3 invariant 4). Each body runs
//! inside an [`audit::CallbackScope`](crate::audit) so allocation
//! instrumentation can attribute heap use to callbacks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use cpal::{FromSample, Sample};

use crate::audit::CallbackScope;
use crate::device::DeviceErrorKind;
use crate::ring::{RingConsumer, RingProducer};

/// Lock-free counters shared by one stream's callbacks and the engine.
#[derive(Default)]
pub(crate) struct StreamCounters {
    pub(crate) callbacks: AtomicU64,
    /// Capture: samples discarded because the engine fell behind.
    pub(crate) overrun_samples: AtomicU64,
    /// Playback: times the ring ran dry while audio was flowing.
    pub(crate) underruns: AtomicU64,
    /// Playback: samples of silence inserted for those underruns.
    pub(crate) underrun_samples: AtomicU64,
    /// Device-reported buffer under/overruns.
    pub(crate) xruns: AtomicU64,
    /// First fatal stream error, as `DeviceErrorKind as u8 + 1`; 0 = none.
    fault: AtomicU8,
}

impl StreamCounters {
    /// Records a stream error reported by the backend. Rerouting to a new
    /// default device and real-time scheduling refusals keep the stream
    /// running; xruns are counted; anything else stops the stream.
    pub(crate) fn record_error(&self, kind: cpal::ErrorKind) {
        match kind {
            cpal::ErrorKind::DeviceChanged | cpal::ErrorKind::RealtimeDenied => {}
            cpal::ErrorKind::Xrun => {
                self.xruns.fetch_add(1, Ordering::Relaxed);
            }
            other => {
                let code = DeviceErrorKind::from_cpal(other) as u8 + 1;
                let _ = self
                    .fault
                    .compare_exchange(0, code, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
    }

    /// The fatal error recorded for this stream, if any.
    pub(crate) fn fault(&self) -> Option<DeviceErrorKind> {
        match self.fault.load(Ordering::Acquire) {
            0 => None,
            code => Some(DeviceErrorKind::from_code(code - 1)),
        }
    }
}

/// Capture callback: device samples → f32 → capture ring.
pub(crate) struct InputCallback {
    ring: RingProducer,
    counters: Arc<StreamCounters>,
    enabled: Arc<AtomicBool>,
}

impl InputCallback {
    pub(crate) fn new(
        ring: RingProducer,
        counters: Arc<StreamCounters>,
        enabled: Arc<AtomicBool>,
    ) -> Self {
        Self {
            ring,
            counters,
            enabled,
        }
    }

    pub(crate) fn process<T>(&mut self, data: &[T])
    where
        T: Sample,
        f32: FromSample<T>,
    {
        let _scope = CallbackScope::enter();
        self.counters.callbacks.fetch_add(1, Ordering::Relaxed);
        if !self.enabled.load(Ordering::Acquire) {
            return;
        }
        let whole = data.len() / self.ring.channels() * self.ring.channels();
        let samples = data[..whole].iter().map(|&s| f32::from_sample(s));
        let discarded = self.ring.push_overwrite(samples);
        if discarded > 0 {
            self.counters
                .overrun_samples
                .fetch_add(discarded as u64, Ordering::Relaxed);
        }
    }
}

/// Playback callback: playback ring → device samples; silence when empty.
pub(crate) struct OutputCallback {
    ring: RingConsumer,
    counters: Arc<StreamCounters>,
    /// The previous callback was fully supplied, so running dry now is an
    /// underrun rather than continued idle silence.
    flowing: bool,
}

impl OutputCallback {
    pub(crate) fn new(ring: RingConsumer, counters: Arc<StreamCounters>) -> Self {
        Self {
            ring,
            counters,
            flowing: false,
        }
    }

    pub(crate) fn process<T>(&mut self, out: &mut [T])
    where
        T: Sample + FromSample<f32>,
    {
        let _scope = CallbackScope::enter();
        let upper = 1.0_f32.next_down();
        self.counters.callbacks.fetch_add(1, Ordering::Relaxed);
        let written = self
            .ring
            .pop_map(out, |s| T::from_sample(s.clamp(-1.0, upper)));
        let missing = out.len() - written;
        if missing > 0 {
            out[written..].fill(T::EQUILIBRIUM);
            if self.flowing || written > 0 {
                self.counters.underruns.fetch_add(1, Ordering::Relaxed);
                self.counters
                    .underrun_samples
                    .fetch_add(missing as u64, Ordering::Relaxed);
            }
        }
        self.flowing = missing == 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alloc_counter::thread_callback_heap_ops;
    use crate::ring::sample_ring;
    use cpal::{I24, U24};

    fn input(channels: usize, frames: usize) -> (InputCallback, RingConsumer, Arc<StreamCounters>) {
        let (producer, consumer) = sample_ring(channels, frames);
        let counters = Arc::new(StreamCounters::default());
        (
            InputCallback::new(
                producer,
                Arc::clone(&counters),
                Arc::new(AtomicBool::new(true)),
            ),
            consumer,
            counters,
        )
    }

    fn output(
        channels: usize,
        frames: usize,
    ) -> (OutputCallback, RingProducer, Arc<StreamCounters>) {
        let (producer, consumer) = sample_ring(channels, frames);
        let counters = Arc::new(StreamCounters::default());
        (
            OutputCallback::new(consumer, Arc::clone(&counters)),
            producer,
            counters,
        )
    }

    #[test]
    fn every_device_format_converts_to_unit_range_f32() {
        let (mut callback, mut ring, _) = input(1, 64);
        callback.process(&[i16::MIN, 0, i16::MAX / 2]);
        callback.process(&[u8::MIN, 128, u8::MAX]);
        callback.process(&[I24::new(-(1 << 23)).unwrap(), I24::new(1 << 22).unwrap()]);
        callback.process(&[U24::new(1 << 23).unwrap()]);
        callback.process(&[i32::MAX, i8::MIN as i32 * (1 << 24)]);
        callback.process(&[0.25f32, -0.5]);
        callback.process(&[0.75f64]);
        let mut out = [0.0; 64];
        let n = ring.pop(&mut out);
        let expected = [
            -1.0, 0.0, 0.5, -1.0, 0.0, 0.992, -1.0, 0.5, 0.0, 1.0, -1.0, 0.25, -0.5, 0.75,
        ];
        assert_eq!(n, expected.len());
        for (got, want) in out[..n].iter().zip(expected) {
            assert!((got - want).abs() < 0.01, "{got} vs {want}");
        }
    }

    #[test]
    fn input_overrun_discards_oldest_and_counts_it() {
        let (mut callback, mut ring, counters) = input(2, 4);
        callback.process(&[1i16; 6]);
        callback.process(&[2i16; 6]);
        assert_eq!(counters.overrun_samples.load(Ordering::Relaxed), 4);
        assert_eq!(counters.callbacks.load(Ordering::Relaxed), 2);
        let mut out = [0.0; 8];
        assert_eq!(ring.pop(&mut out), 8);
        assert!(out[..2].iter().all(|&s| s > 0.0 && s < 0.0001));
        assert!(out[2..].iter().all(|&s| s > 0.00005));
    }

    #[test]
    fn input_ignores_a_trailing_partial_frame() {
        let (mut callback, ring, _) = input(2, 4);
        callback.process(&[0.1f32, 0.2, 0.3]);
        assert_eq!(ring.len(), 2);
    }

    #[test]
    fn output_converts_clamps_and_fills_silence_on_underrun() {
        let (mut callback, mut ring, counters) = output(2, 8);
        ring.push_available([0.5, -2.0, 1.5, 0.0].into_iter());
        let mut out = [7u16; 6];
        callback.process(&mut out);
        assert_eq!(out, [49152, 0, 65535, 32768, 32768, 32768]);
        // Partially supplied: one underrun of two samples.
        assert_eq!(counters.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(counters.underrun_samples.load(Ordering::Relaxed), 2);
        // Idle silence after running dry is not counted again.
        let mut idle = [1.0f32; 4];
        callback.process(&mut idle);
        assert_eq!(idle, [0.0; 4]);
        assert_eq!(counters.underruns.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn output_saturates_24_bit_integer_samples() {
        let (mut callback, mut ring, _) = output(1, 3);
        ring.push_available([1.0, 1.5, -1.0].into_iter());
        let mut signed = [I24::new(0).unwrap(); 3];
        callback.process(&mut signed);
        assert_eq!(
            signed,
            [
                I24::new(8_388_607).unwrap(),
                I24::new(8_388_607).unwrap(),
                I24::new(-8_388_608).unwrap(),
            ]
        );

        let (mut callback, mut ring, _) = output(1, 3);
        ring.push_available([1.0, 1.5, -1.0].into_iter());
        let mut unsigned = [U24::new(0).unwrap(); 3];
        callback.process(&mut unsigned);
        assert_eq!(
            unsigned,
            [
                U24::new(16_777_215).unwrap(),
                U24::new(16_777_215).unwrap(),
                U24::new(0).unwrap(),
            ]
        );
    }

    #[test]
    fn output_counts_a_dry_ring_after_a_fully_supplied_callback() {
        let (mut callback, mut ring, counters) = output(1, 8);
        let mut out = [0.0f32; 4];
        callback.process(&mut out);
        assert_eq!(
            counters.underruns.load(Ordering::Relaxed),
            0,
            "startup silence"
        );
        ring.push_available([0.1; 4].into_iter());
        callback.process(&mut out);
        assert_eq!(counters.underruns.load(Ordering::Relaxed), 0);
        callback.process(&mut out);
        assert_eq!(counters.underruns.load(Ordering::Relaxed), 1);
        assert_eq!(counters.underrun_samples.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn errors_are_classified_and_only_the_first_fault_is_kept() {
        let counters = StreamCounters::default();
        counters.record_error(cpal::ErrorKind::DeviceChanged);
        counters.record_error(cpal::ErrorKind::RealtimeDenied);
        assert_eq!(counters.fault(), None);
        counters.record_error(cpal::ErrorKind::Xrun);
        assert_eq!(counters.xruns.load(Ordering::Relaxed), 1);
        assert_eq!(counters.fault(), None);
        counters.record_error(cpal::ErrorKind::DeviceNotAvailable);
        counters.record_error(cpal::ErrorKind::PermissionDenied);
        assert_eq!(counters.fault(), Some(DeviceErrorKind::Disconnected));
    }

    /// The callback bodies, instantiated for every sample type, under the
    /// counting allocator: no heap operation may happen in any of them,
    /// including overrun, underrun, and wrap-around paths.
    #[test]
    fn callbacks_never_touch_the_heap() {
        fn exercise<T>(sample: T)
        where
            T: Sample + FromSample<f32> + Copy,
            f32: FromSample<T>,
        {
            let (mut input_cb, mut capture, _) = input(2, 480);
            let (mut output_cb, mut playback, _) = output(2, 480);
            let device_buffer = [sample; 1_024];
            let mut device_out = [sample; 1_024];
            let mut scratch = vec![0.0f32; 960];
            let before = thread_callback_heap_ops();
            for round in 0..50 {
                input_cb.process(&device_buffer[..(round % 7 + 1) * 128]);
                let n = capture.pop(&mut scratch[..(round % 3 + 1) * 200]);
                playback.push_available(scratch[..n].iter().copied());
                output_cb.process(&mut device_out[..(round % 5 + 1) * 96]);
            }
            assert_eq!(thread_callback_heap_ops(), before);
        }
        exercise(0i8);
        exercise(1i16);
        exercise(I24::new(5).unwrap());
        exercise(1i32);
        exercise(1i64);
        exercise(1u8);
        exercise(1u16);
        exercise(U24::new(5).unwrap());
        exercise(1u32);
        exercise(1u64);
        exercise(0.1f32);
        exercise(0.1f64);
    }
}
