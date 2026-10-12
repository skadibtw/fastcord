use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use super::{ColorDescription, Nv12};
use crate::codec::{DecodedFrame, check_dimensions};

pub const MAX_FRAME_BUFFERS: usize = 3;
pub const MAX_FRAME_POOL_BYTES: usize = 48 * 1024 * 1024;

/// The one owned pixel representation selected for a playback stage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameFormat {
    /// Packed visible NV12 planes; no RGB conversion occurs on this path.
    Nv12,
    /// Tightly packed opaque BGRA, suitable for the tiny-skia CPU fallback.
    Bgra,
}

/// Why a decoded picture could not be retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseError {
    /// Empty, overflowing or over the codec's dimension/area bounds.
    InvalidDimensions,
    /// Retaining the picture would exceed the 48 MiB capacity budget.
    CapacityLimit,
    /// All three buffers still have outstanding leases.
    Exhausted,
    /// The allocator could not supply the bounded pixel buffer.
    AllocationFailed,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDimensions => "decoded frame dimensions exceed playback limits",
            Self::CapacityLimit => "decoded frame pool capacity limit exceeded",
            Self::Exhausted => "all decoded frame buffers are leased",
            Self::AllocationFailed => "decoded frame buffer allocation failed",
        })
    }
}

impl std::error::Error for LeaseError {}

struct FrameBuffer {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    color: ColorDescription,
    timestamp: Duration,
    serial: u64,
    format: FrameFormat,
}

impl fmt::Debug for FrameBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameBuffer")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("color", &self.color)
            .field("timestamp", &self.timestamp)
            .field("serial", &self.serial)
            .field("format", &self.format)
            .field("bytes", &self.pixels.len())
            .field("capacity", &self.pixels.capacity())
            .finish()
    }
}

/// An immutable frame handle; cloning it clones only an `Arc`, never pixels.
///
/// The pool retains one reference per slot. Dropping the last external lease
/// makes that slot recyclable; only `FramePool::lease` on the decoder owner
/// can obtain mutable access. Dropping the pool leaves outstanding leases
/// valid, and their last drop then releases the pixels and slot allocation.
#[derive(Clone, Debug)]
pub struct FrameLease(Arc<FrameBuffer>);

impl FrameLease {
    pub fn width(&self) -> u32 {
        self.0.width
    }

    pub fn height(&self) -> u32 {
        self.0.height
    }

    pub fn color(&self) -> ColorDescription {
        self.0.color
    }

    pub fn timestamp(&self) -> Duration {
        self.0.timestamp
    }

    /// Increasing identity within this pool, independent of presentation time.
    pub fn serial(&self) -> u64 {
        self.0.serial
    }

    pub fn format(&self) -> FrameFormat {
        self.0.format
    }

    pub fn pixels(&self) -> &[u8] {
        &self.0.pixels
    }

    /// Packed Y/UV views, or `None` on the BGRA fallback path.
    pub fn nv12(&self) -> Option<Nv12<'_>> {
        if self.0.format != FrameFormat::Nv12 {
            return None;
        }
        // Dimensions and packed length were checked before publishing the slot.
        Nv12::packed(self.0.width, self.0.height, &self.0.pixels).ok()
    }
}

/// Decoder-thread-owned pool, bounded by both slot count and pixel capacities.
///
/// The fixed three-slot array does not allocate. Slot `Arc`s and pixel buffers
/// are created lazily on the decoding thread and reused after warmup, including
/// the `Arc` allocation itself. A live lease is never overwritten or evicted.
/// Resolution changes reclaim only unleased capacities before growing another
/// buffer; neither retained spare capacity nor leased buffers escape the cap.
#[derive(Debug)]
pub struct FramePool {
    format: FrameFormat,
    slots: [Option<Arc<FrameBuffer>>; MAX_FRAME_BUFFERS],
    next_serial: u64,
}

impl FramePool {
    pub fn new(format: FrameFormat) -> Self {
        Self {
            format,
            slots: std::array::from_fn(|_| None),
            next_serial: 1,
        }
    }

    /// Number of initialized recyclable slots, including currently leased ones.
    pub fn buffers(&self) -> usize {
        self.slots.iter().flatten().count()
    }

    /// Sum of all owned pixel capacities, including leased and unused buffers.
    pub fn capacity_bytes(&self) -> usize {
        self.slots
            .iter()
            .flatten()
            .map(|slot| slot.pixels.capacity())
            .sum()
    }

    /// Retains the visible pixels and metadata during a borrowed decode callback.
    /// NV12 copies each visible row once, stripping padding; BGRA converts
    /// directly from the borrowed NV12 without an intermediate YUV copy.
    pub fn lease(&mut self, frame: DecodedFrame<'_>) -> Result<FrameLease, LeaseError> {
        let width = frame.image.width();
        let height = frame.image.height();
        check_dimensions(width, height).map_err(|_| LeaseError::InvalidDimensions)?;
        let required = match self.format {
            FrameFormat::Nv12 => frame.image.packed_len(),
            FrameFormat::Bgra => (width as usize)
                .checked_mul(height as usize)
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(LeaseError::InvalidDimensions)?,
        };
        if required > MAX_FRAME_POOL_BYTES {
            return Err(LeaseError::CapacityLimit);
        }
        // Prefer the smallest sufficient unleased capacity. Holding our one
        // pool reference makes strong_count==1 stable against concurrent drops:
        // nobody can acquire another external reference once it reaches one.
        let fitting = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let slot = slot.as_ref()?;
                (Arc::strong_count(slot) == 1 && slot.pixels.capacity() >= required)
                    .then_some((index, slot.pixels.capacity()))
            })
            .min_by_key(|&(_, capacity)| capacity)
            .map(|(index, _)| index);
        let index = if let Some(index) = fitting {
            index
        } else {
            let index = self
                .slots
                .iter()
                .position(|slot| {
                    slot.as_ref()
                        .is_none_or(|slot| Arc::strong_count(slot) == 1)
                })
                .ok_or(LeaseError::Exhausted)?;
            self.make_room(index, required)?;
            index
        };
        let slot = self.slots[index].get_or_insert_with(|| {
            Arc::new(FrameBuffer {
                pixels: Vec::new(),
                width: 0,
                height: 0,
                color: ColorDescription::default(),
                timestamp: Duration::ZERO,
                serial: 0,
                format: self.format,
            })
        });
        let buffer = Arc::get_mut(slot).expect("only an unleased slot can be selected");
        if buffer.pixels.capacity() < required {
            buffer
                .pixels
                .try_reserve_exact(required)
                .map_err(|_| LeaseError::AllocationFailed)?;
        }
        match self.format {
            FrameFormat::Nv12 => {
                buffer.pixels.clear();
                frame.image.copy_packed_into(&mut buffer.pixels);
            }
            FrameFormat::Bgra => {
                buffer.pixels.resize(required, 0);
                convert_bgra(frame.image, frame.color, &mut buffer.pixels);
            }
        }
        buffer.width = width;
        buffer.height = height;
        buffer.color = frame.color;
        buffer.timestamp = frame.timestamp;
        buffer.serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1);
        Ok(FrameLease(Arc::clone(slot)))
    }

    fn make_room(&mut self, index: usize, required: usize) -> Result<(), LeaseError> {
        let leased_bytes: usize = self
            .slots
            .iter()
            .flatten()
            .filter(|slot| Arc::strong_count(slot) != 1)
            .map(|slot| slot.pixels.capacity())
            .sum();
        if required > MAX_FRAME_POOL_BYTES - leased_bytes {
            return Err(LeaseError::CapacityLimit);
        }
        // Release the selected undersized allocation before reserving another,
        // so growth never briefly owns both old and new pixel capacities.
        if let Some(slot) = &mut self.slots[index] {
            Arc::get_mut(slot)
                .expect("selected slot is unleased")
                .pixels = Vec::new();
        }
        while required > MAX_FRAME_POOL_BYTES - self.capacity_bytes() {
            let reclaim = self
                .slots
                .iter()
                .enumerate()
                .filter_map(|(candidate, slot)| {
                    let slot = slot.as_ref()?;
                    (candidate != index
                        && Arc::strong_count(slot) == 1
                        && slot.pixels.capacity() != 0)
                        .then_some((candidate, slot.pixels.capacity()))
                })
                .max_by_key(|&(_, capacity)| capacity)
                .map(|(candidate, _)| candidate)
                .expect("the checked budget can be met by reclaiming unleased slots");
            let slot = self.slots[reclaim]
                .as_mut()
                .expect("reclaimable slot exists");
            Arc::get_mut(slot)
                .expect("reclaimed slot is unleased")
                .pixels = Vec::new();
        }
        Ok(())
    }
}

fn convert_bgra(image: Nv12<'_>, color: ColorDescription, out: &mut [u8]) {
    let rows = color.transform_rows();
    // Express the same shader transform in byte units once. One chroma
    // contribution serves its whole 2x2 block, including partial edge blocks.
    let biases = rows.map(|row| row[3] * 255.0);
    let width = image.width() as usize;
    let height = image.height() as usize;
    for uv_y in 0..image.chroma_rows() {
        let top = uv_y * 2;
        let luma = [
            Some(image.luma_row(top)),
            (top + 1 < height).then(|| image.luma_row(top + 1)),
        ];
        for (uv_x, pair) in image.chroma_row(uv_y).as_chunks::<2>().0.iter().enumerate() {
            let u = f32::from(pair[0]);
            let v = f32::from(pair[1]);
            let chroma_rgb = [
                rows[0][2] * v + biases[0],
                rows[1][1] * u + rows[1][2] * v + biases[1],
                rows[2][1] * u + biases[2],
            ];
            for (dy, luma) in luma.into_iter().enumerate() {
                let Some(luma) = luma else {
                    continue;
                };
                for (x, &sample) in luma.iter().enumerate().skip(uv_x * 2).take(2) {
                    let y = rows[0][0] * f32::from(sample);
                    let rgb = chroma_rgb.map(|chroma| (y + chroma).round().clamp(0.0, 255.0) as u8);
                    let offset = ((top + dy) * width + x) * 4;
                    out[offset..offset + 4].copy_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
