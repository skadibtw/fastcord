//! Media Foundation H.264 encode/decode (SPEC §8.2, §8.5; ADR 0001).
//!
//! Encoding prefers a vendor hardware encoder transform (asynchronous, on the
//! GPU) and falls back to Microsoft's software encoder transform. Decoding
//! uses Microsoft's H.264 decoder transform with D3D11VA on a hardware
//! adapter, falling back to the same transform in software. Every codec
//! reports the backend it actually uses.
//!
//! Codec objects belong to the thread that opened them: COM and Media
//! Foundation are initialized per codec on that thread.
//!
//! Unsafe code is confined to this module: it is the Media Foundation, COM,
//! and Direct3D 11 FFI.
#![allow(unsafe_code)]

mod d3d;
mod decoder;
mod encoder;
mod runtime;
mod transform;

use std::time::Duration;

use windows::Win32::Graphics::Dxgi::IDXGIAdapter1;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_ADAPTER_LUID, MFT_ENUM_FLAG, MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_LOCALMFT,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::core::GUID;

pub use decoder::MfDecoder;
pub use encoder::MfEncoder;

use self::d3d::{Adapter, DeviceManager, hardware_adapters};
use self::runtime::Session;
use self::transform::Transform;
use crate::codec::{Backend, BackendKind, BackendPreference, CodecError, EncoderConfig};
use crate::frame::Nv12;
use crate::select::{Candidate, OpenError, Selected, select};

/// Reported as [`Backend::api`].
pub const API: &str = "Media Foundation";

/// Opens an H.264 encoder: hardware encoder transforms in merit order, then
/// software ones, as `preference` allows. The result reports the backend and
/// any candidate that failed before it.
pub fn open_encoder(
    config: EncoderConfig,
    preference: BackendPreference,
) -> Result<Selected<MfEncoder>, OpenError> {
    config.validate().map_err(OpenError::Invalid)?;
    let session = Session::start().map_err(OpenError::Discovery)?;
    let candidates = encoder_candidates(&session, preference).map_err(OpenError::Discovery)?;
    select(preference, candidates, |(activate, backend)| {
        let session = Session::start()?;
        let transform = Transform::activate(activate)?;
        MfEncoder::open(session, transform, backend, config)
    })
}

/// Opens an H.264 decoder: Microsoft's decoder transform with D3D11VA on each
/// hardware adapter (the default first), then in software, as `preference`
/// allows.
pub fn open_decoder(preference: BackendPreference) -> Result<Selected<MfDecoder>, OpenError> {
    let session = Session::start().map_err(OpenError::Discovery)?;
    let candidates = decoder_candidates(&session, preference).map_err(OpenError::Discovery)?;
    select(preference, candidates, |(activate, adapter, backend)| {
        let session = Session::start()?;
        let device = adapter
            .map(|adapter| DeviceManager::new(&session, &adapter))
            .transpose()?;
        let transform = Transform::activate(activate)?;
        MfDecoder::open(session, transform, backend, device, preference)
    })
}

type EncoderCandidate = Candidate<(IMFActivate, Backend)>;
type DecoderCandidate = Candidate<(IMFActivate, Option<IDXGIAdapter1>, Backend)>;

const HARDWARE: MFT_ENUM_FLAG =
    MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
const SOFTWARE: MFT_ENUM_FLAG = MFT_ENUM_FLAG(
    MFT_ENUM_FLAG_SYNCMFT.0 | MFT_ENUM_FLAG_LOCALMFT.0 | MFT_ENUM_FLAG_SORTANDFILTER.0,
);

fn encoder_candidates(
    session: &Session,
    preference: BackendPreference,
) -> Result<Vec<EncoderCandidate>, CodecError> {
    let encoders = |flags| {
        session.transforms(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            MFVideoFormat_NV12,
            MFVideoFormat_H264,
        )
    };
    let mut candidates = Vec::new();
    if preference.allows(BackendKind::Hardware) {
        let adapters = hardware_adapters()?;
        for activate in encoders(HARDWARE)? {
            let transform = friendly_name(&activate, "hardware H.264 encoder");
            let name = match encoder_adapter(&activate, &adapters) {
                Some(adapter) => format!("{transform} on {}", adapter.name),
                None => transform,
            };
            candidates.push(candidate(BackendKind::Hardware, name, activate));
        }
    }
    if preference.allows(BackendKind::Software) {
        for activate in encoders(SOFTWARE)? {
            let name = friendly_name(&activate, "software H.264 encoder");
            candidates.push(candidate(BackendKind::Software, name, activate));
        }
    }
    Ok(candidates)
}

fn candidate(kind: BackendKind, name: String, activate: IMFActivate) -> EncoderCandidate {
    Candidate {
        kind,
        source: (activate, backend(kind, name.clone())),
        name,
    }
}

fn decoder_candidates(
    session: &Session,
    preference: BackendPreference,
) -> Result<Vec<DecoderCandidate>, CodecError> {
    // Each candidate needs its own activation object, so the software
    // decoders are enumerated once per use.
    let decoders = || {
        session.transforms(
            MFT_CATEGORY_VIDEO_DECODER,
            SOFTWARE,
            MFVideoFormat_H264,
            MFVideoFormat_NV12,
        )
    };
    let mut candidates = Vec::new();
    if preference.allows(BackendKind::Hardware) {
        for adapter in hardware_adapters()? {
            for activate in decoders()? {
                let transform = friendly_name(&activate, "H.264 decoder");
                let name = format!("{transform} with D3D11VA on {}", adapter.name);
                candidates.push(Candidate {
                    kind: BackendKind::Hardware,
                    source: (
                        activate,
                        Some(adapter.adapter.clone()),
                        backend(BackendKind::Hardware, name.clone()),
                    ),
                    name,
                });
            }
        }
    }
    if preference.allows(BackendKind::Software) {
        for activate in decoders()? {
            let name = friendly_name(&activate, "H.264 decoder");
            candidates.push(Candidate {
                kind: BackendKind::Software,
                source: (activate, None, backend(BackendKind::Software, name.clone())),
                name,
            });
        }
    }
    Ok(candidates)
}

fn backend(kind: BackendKind, name: String) -> Backend {
    Backend {
        kind,
        api: API,
        name,
    }
}

fn friendly_name(attributes: &IMFAttributes, fallback: &str) -> String {
    string_attribute(attributes, &MFT_FRIENDLY_NAME_Attribute)
        .unwrap_or_else(|| fallback.to_owned())
}

fn string_attribute(attributes: &IMFAttributes, key: &GUID) -> Option<String> {
    // SAFETY: COM calls on a valid interface; the buffer holds the reported
    // length plus the terminator.
    unsafe {
        let length = attributes.GetStringLength(key).ok()? as usize;
        let mut text = vec![0u16; length + 1];
        attributes.GetString(key, &mut text, None).ok()?;
        Some(String::from_utf16_lossy(&text[..length]).trim().to_owned())
    }
}

/// The adapter a hardware encoder runs on: by the adapter LUID where the
/// driver states one, otherwise the only adapter of the transform's vendor.
fn encoder_adapter<'a>(attributes: &IMFAttributes, adapters: &'a [Adapter]) -> Option<&'a Adapter> {
    if let Some(luid) = adapter_luid(attributes) {
        return adapters
            .iter()
            .find(|adapter| luid_value(adapter.luid) == luid);
    }
    let vendor = string_attribute(attributes, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute)?;
    let vendor = parse_vendor_id(&vendor)?;
    let mut matching = adapters
        .iter()
        .filter(|adapter| adapter.vendor_id == vendor);
    match (matching.next(), matching.next()) {
        (Some(adapter), None) => Some(adapter),
        _ => None,
    }
}

/// Parses a hardware transform's vendor ID attribute (`VEN_1002`).
fn parse_vendor_id(text: &str) -> Option<u32> {
    let digits = text.strip_prefix("VEN_")?;
    if digits.len() != 4 {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// The adapter a hardware transform runs on.
fn adapter_luid(attributes: &IMFAttributes) -> Option<u64> {
    // SAFETY: COM calls on a valid interface; the blob is read into eight
    // bytes, its documented size.
    unsafe {
        if let Ok(luid) = attributes.GetUINT64(&MFT_ENUM_ADAPTER_LUID) {
            return Some(luid);
        }
        let mut bytes = [0u8; 8];
        attributes
            .GetBlob(&MFT_ENUM_ADAPTER_LUID, &mut bytes, None)
            .ok()?;
        Some(u64::from_le_bytes(bytes))
    }
}

fn luid_value(luid: windows::Win32::Foundation::LUID) -> u64 {
    (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart)
}

/// Copies the visible pixels of `frame` into `buffer` as packed NV12.
fn pack(frame: &Nv12<'_>, buffer: &mut [u8]) {
    let (luma, chroma) = buffer.split_at_mut(frame.luma_row_bytes() * frame.height() as usize);
    for (row, target) in luma.chunks_exact_mut(frame.luma_row_bytes()).enumerate() {
        target.copy_from_slice(frame.luma_row(row));
    }
    for (row, target) in chroma
        .chunks_exact_mut(frame.chroma_row_bytes())
        .enumerate()
    {
        target.copy_from_slice(frame.chroma_row(row));
    }
}

/// Media Foundation time: 100-nanosecond units.
fn to_hns(time: Duration) -> i64 {
    i64::try_from(time.as_nanos() / 100).unwrap_or(i64::MAX)
}

fn from_hns(time: i64) -> Duration {
    Duration::from_nanos(u64::try_from(time).unwrap_or(0).saturating_mul(100))
}

#[cfg(test)]
mod tests;
