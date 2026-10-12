//! Windows.Graphics.Capture screen and window capture (SPEC §8.4; ADR 0001).
//!
//! Every native session, its Direct3D 11 immediate context and its COM/WinRT
//! apartment stay on one MTA owner thread (see [`runtime::Native`]); only the
//! stop and wake events and the pacing atomics cross threads. The OS capture
//! border is deliberately left enabled, so the user can always see what is
//! being shared. The shared surface is converted to NV12 at the output size on
//! the GPU (a D3D11 video processor, BT.709 limited range) and read back, with
//! the CPU converter in [`super::convert`] as the fallback.
//!
//! Unsafe code is confined to this module tree: it is the Win32, WinRT and
//! Direct3D 11 FFI.
#![allow(unsafe_code)]

mod gpu;
mod runtime;
mod sources;
#[cfg(any(test, feature = "test-support"))]
pub mod test_window;
mod wgc;

#[cfg(test)]
mod tests;

pub(super) use runtime::Native;
pub(super) use sources::list_sources;
pub(super) use wgc::{availability, start};
