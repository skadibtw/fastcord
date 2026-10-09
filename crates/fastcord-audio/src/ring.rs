//! Preallocated single-producer/single-consumer PCM ring.
//!
//! Samples are stored as `f32` bit patterns in atomics, so neither side needs
//! `unsafe` or a lock, and a consumer racing an overwriting producer reads
//! stale values instead of causing undefined behavior. Positions are monotonic
//! counters; the physical capacity is a power of two and a separate `limit`
//! (≤ capacity) is the logical bound used for the 100 ms hard cap.
//!
//! The capture side uses [`RingProducer::push_overwrite`]: when the ring is
//! full, the oldest unread samples are discarded so latency never grows past
//! the limit. The consumer detects a concurrent discard (its claimed read
//! position moved) and retries, so it never returns torn data.
//!
//! All operations work in whole frames: callers push and pop multiples of the
//! channel count fixed at construction.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering, fence};

struct Shared {
    slots: Box<[AtomicU32]>,
    mask: usize,
    limit: usize,
    channels: usize,
    /// Next position the producer writes. Only the producer stores it.
    write: AtomicUsize,
    /// Next position the consumer reads. The consumer advances it after a
    /// read; an overwriting producer advances it to discard old samples.
    read: AtomicUsize,
}

/// Creates a ring holding at most `limit_frames` frames of `channels`
/// interleaved samples, allocating all storage up front.
///
/// # Panics
///
/// Panics if `channels` or `limit_frames` is zero or the size overflows.
pub(crate) fn sample_ring(channels: usize, limit_frames: usize) -> (RingProducer, RingConsumer) {
    assert!(channels > 0 && limit_frames > 0, "empty ring");
    let limit = channels
        .checked_mul(limit_frames)
        .expect("ring size overflows");
    let capacity = limit.checked_next_power_of_two().expect("ring too large");
    let slots = (0..capacity).map(|_| AtomicU32::new(0)).collect();
    let shared = Arc::new(Shared {
        slots,
        mask: capacity - 1,
        limit,
        channels,
        write: AtomicUsize::new(0),
        read: AtomicUsize::new(0),
    });
    (
        RingProducer {
            shared: Arc::clone(&shared),
        },
        RingConsumer { shared },
    )
}

impl Shared {
    fn fill(&self) -> usize {
        let read = self.read.load(Ordering::Acquire);
        let write = self.write.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }
}

/// Writing half of a sample ring. Not cloneable: there is exactly one producer.
pub(crate) struct RingProducer {
    shared: Arc<Shared>,
}

/// Reading half of a sample ring. Not cloneable: there is exactly one consumer.
pub(crate) struct RingConsumer {
    shared: Arc<Shared>,
}

impl RingProducer {
    /// Interleaved channel count.
    pub(crate) fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Maximum buffered samples.
    pub(crate) fn limit(&self) -> usize {
        self.shared.limit
    }

    /// Buffered (unread) samples.
    pub(crate) fn len(&self) -> usize {
        self.shared.fill()
    }

    /// Samples that can be written without discarding anything.
    pub(crate) fn free(&self) -> usize {
        self.shared.limit.saturating_sub(self.shared.fill())
    }

    /// Discards every unread sample. A concurrent consumer retries against the advanced
    /// read position, just as it does when capture overwrites old samples.
    pub(crate) fn clear(&mut self) {
        let write = self.shared.write.load(Ordering::Acquire);
        let mut read = self.shared.read.load(Ordering::Acquire);
        while read != write {
            match self.shared.read.compare_exchange_weak(
                read,
                write,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(current) => read = current,
            }
        }
    }
    /// Writes all `samples`, discarding the oldest unread samples when the
    /// ring is full. If more than `limit` samples are offered, only the newest
    /// `limit` are kept. Returns the number of samples discarded (old or new).
    ///
    /// Real-time safe: no allocation, no lock, bounded work.
    pub(crate) fn push_overwrite<I>(&mut self, samples: I) -> usize
    where
        I: ExactSizeIterator<Item = f32>,
    {
        let shared = &*self.shared;
        let offered = samples.len();
        debug_assert_eq!(offered % shared.channels, 0, "partial frame pushed");
        // Keep only the newest `limit` samples of an oversized push; `limit`
        // is a whole number of frames, so frame alignment is preserved.
        let skip = offered.saturating_sub(shared.limit);
        let count = offered - skip;
        let write = shared.write.load(Ordering::Relaxed);
        let mut discarded = skip;
        let mut read = shared.read.load(Ordering::Acquire);
        loop {
            let fill = write.wrapping_sub(read);
            let excess = (fill + count).saturating_sub(shared.limit);
            if excess == 0 {
                break;
            }
            match shared.read.compare_exchange_weak(
                read,
                read.wrapping_add(excess),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    discarded += excess;
                    break;
                }
                Err(current) => read = current,
            }
        }
        // Pairs with the consumer's acquire fence: a consumer that observes
        // any sample written below also observes the read-position change
        // above, so its claim on the overwritten region fails.
        fence(Ordering::Release);
        for (offset, sample) in samples.skip(skip).enumerate() {
            shared.slots[write.wrapping_add(offset) & shared.mask]
                .store(sample.to_bits(), Ordering::Relaxed);
        }
        shared
            .write
            .store(write.wrapping_add(count), Ordering::Release);
        discarded
    }

    /// Writes whole frames from `samples` while space remains and returns the
    /// number of samples written. Never discards buffered data.
    pub(crate) fn push_available<I>(&mut self, samples: I) -> usize
    where
        I: Iterator<Item = f32>,
    {
        let shared = &*self.shared;
        let space = self.free() / shared.channels * shared.channels;
        let write = shared.write.load(Ordering::Relaxed);
        let mut written = 0;
        for sample in samples.take(space) {
            shared.slots[write.wrapping_add(written) & shared.mask]
                .store(sample.to_bits(), Ordering::Relaxed);
            written += 1;
        }
        let written = written / shared.channels * shared.channels;
        shared
            .write
            .store(write.wrapping_add(written), Ordering::Release);
        written
    }
}

impl RingConsumer {
    /// Interleaved channel count.
    pub(crate) fn channels(&self) -> usize {
        self.shared.channels
    }

    /// Buffered (unread) samples.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.shared.fill()
    }

    /// Reads up to `out.len()` samples (rounded down to whole frames),
    /// converting each with `map`, and returns how many were written to `out`.
    ///
    /// If the producer discards part of the claimed region meanwhile, the read
    /// restarts from the new oldest sample. Real-time safe: no allocation, no
    /// lock; retries only happen while an overwriting producer is active.
    pub(crate) fn pop_map<T>(&mut self, out: &mut [T], mut map: impl FnMut(f32) -> T) -> usize {
        let shared = &*self.shared;
        let wanted = out.len() / shared.channels * shared.channels;
        loop {
            let read = shared.read.load(Ordering::Acquire);
            let write = shared.write.load(Ordering::Acquire);
            let count = write.wrapping_sub(read).min(wanted);
            for (offset, slot) in out[..count].iter_mut().enumerate() {
                let bits =
                    shared.slots[read.wrapping_add(offset) & shared.mask].load(Ordering::Relaxed);
                *slot = map(f32::from_bits(bits));
            }
            fence(Ordering::Acquire);
            if shared
                .read
                .compare_exchange(
                    read,
                    read.wrapping_add(count),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return count;
            }
        }
    }

    /// Reads up to `out.len()` samples (whole frames) unchanged.
    pub(crate) fn pop(&mut self, out: &mut [f32]) -> usize {
        self.pop_map(out, |sample| sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn values(range: std::ops::Range<u32>) -> impl ExactSizeIterator<Item = f32> {
        range.map(|v| v as f32)
    }

    #[test]
    fn capacity_is_a_power_of_two_but_limit_is_exact() {
        let (producer, consumer) = sample_ring(2, 480);
        assert_eq!(producer.limit(), 960);
        assert_eq!(producer.shared.slots.len(), 1024);
        assert_eq!(consumer.len(), 0);
        assert_eq!(producer.free(), 960);
    }

    #[test]
    fn fifo_order_survives_wrapping_the_physical_buffer() {
        let (mut producer, mut consumer) = sample_ring(1, 5);
        let mut out = [0.0; 3];
        let mut next = 0u32;
        let mut expected = 0u32;
        for _ in 0..100 {
            assert_eq!(producer.push_overwrite(values(next..next + 3)), 0);
            next += 3;
            assert_eq!(consumer.pop(&mut out), 3);
            for value in out {
                assert_eq!(value, expected as f32);
                expected += 1;
            }
        }
    }

    #[test]
    fn overwrite_discards_oldest_whole_frames_at_the_limit() {
        let (mut producer, mut consumer) = sample_ring(2, 4);
        assert_eq!(producer.push_overwrite(values(0..6)), 0);
        // Four more samples exceed the 8-sample limit by two: frame (0, 1) goes.
        assert_eq!(producer.push_overwrite(values(6..10)), 2);
        assert_eq!(consumer.len(), 8);
        let mut out = [0.0; 16];
        assert_eq!(consumer.pop(&mut out), 8);
        assert_eq!(&out[..8], &[2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn oversized_push_keeps_only_the_newest_limit() {
        let (mut producer, mut consumer) = sample_ring(2, 2);
        assert_eq!(producer.push_overwrite(values(0..2)), 0);
        // 10 offered into a 4-sample ring: 6 new ones skipped, 2 old dropped.
        assert_eq!(producer.push_overwrite(values(10..20)), 8);
        let mut out = [0.0; 8];
        assert_eq!(consumer.pop(&mut out), 4);
        assert_eq!(&out[..4], &[16.0, 17.0, 18.0, 19.0]);
    }

    #[test]
    fn push_available_never_discards_and_writes_whole_frames() {
        let (mut producer, mut consumer) = sample_ring(2, 3);
        assert_eq!(producer.push_available(values(0..4)), 4);
        // Two samples of space remain; an offer of three writes one frame.
        assert_eq!(producer.push_available(values(4..7)), 2);
        assert_eq!(producer.push_available(values(7..9)), 0);
        let mut out = [0.0; 6];
        assert_eq!(consumer.pop(&mut out), 6);
        assert_eq!(out, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn pop_rounds_down_to_whole_frames() {
        let (mut producer, mut consumer) = sample_ring(2, 4);
        producer.push_overwrite(values(0..6));
        let mut out = [0.0; 5];
        assert_eq!(consumer.pop(&mut out), 4);
        assert_eq!(consumer.len(), 2);
    }

    /// An overwriting producer racing a consumer: every popped batch must be a
    /// contiguous, increasing run of the producer's sequence (no torn or stale
    /// samples), and samples are either delivered or counted as discarded.
    #[test]
    fn concurrent_overwrite_never_yields_torn_or_reordered_samples() {
        const TOTAL: u32 = 1 << 20; // exactly representable as f32
        let (mut producer, mut consumer) = sample_ring(2, 64);
        let writer = thread::spawn(move || {
            let mut next = 0u32;
            let mut discarded = 0u64;
            while next < TOTAL {
                let n = 2 * (1 + next % 37).min((TOTAL - next) / 2);
                discarded += producer.push_overwrite(values(next..next + n)) as u64;
                next += n;
            }
            discarded
        });
        let mut out = [0.0f32; 50];
        let mut last: Option<u32> = None;
        let mut delivered = 0u64;
        loop {
            let n = consumer.pop(&mut out);
            for pair in out[..n].windows(2) {
                assert_eq!(pair[1], pair[0] + 1.0, "batch not contiguous");
            }
            if n > 0 {
                let first = out[0] as u32;
                assert_eq!(first % 2, 0, "frame alignment lost");
                if let Some(last) = last {
                    assert!(first > last, "sequence went backwards");
                }
                last = Some(out[n - 1] as u32);
                delivered += n as u64;
            } else if writer.is_finished() && consumer.len() == 0 {
                break;
            }
        }
        let discarded = writer.join().unwrap();
        assert_eq!(last, Some(TOTAL - 1));
        assert_eq!(delivered + discarded, u64::from(TOTAL));
    }

    #[test]
    fn positions_wrap_around_usize() {
        let (mut producer, mut consumer) = sample_ring(1, 4);
        let start = usize::MAX - 2;
        producer.shared.write.store(start, Ordering::Relaxed);
        producer.shared.read.store(start, Ordering::Relaxed);
        producer.push_overwrite(values(0..3));
        assert_eq!(producer.push_overwrite(values(3..6)), 2);
        let mut out = [0.0; 4];
        assert_eq!(consumer.pop(&mut out), 4);
        assert_eq!(out, [2.0, 3.0, 4.0, 5.0]);
    }
}
