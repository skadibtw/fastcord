//! A soundboard sound as 20 ms Opus packets for the file player (issue #87).
//!
//! Discord stores a server's sounds as short MP3 or Ogg files (at most about 5 s and 512 KB).
//! Ogg Opus passes through unchanged ([`crate::ogg`]); MP3 is decoded by the pure-Rust
//! `symphonia-bundle-mp3` (same family as the AAC decoder, ADR 0008 and 0011), converted to
//! 48 kHz stereo and encoded with libopus. Ogg Vorbis and every other format are
//! [`SoundError::Unsupported`]. Decoding the whole clip up front is bounded by
//! [`MAX_INPUT_BYTES`] in and [`MAX_CLIP_MS`] out (at most about 100 KiB of packets), and
//! runs on a blocking worker in front of the engine, never on the UI thread or in a callback.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use opus2::{Application, Bitrate, Channels, Encoder};
use symphonia_bundle_mp3::{MpaDecoder, MpaReader};
use symphonia_core::codecs::audio::well_known::CODEC_ID_MP3;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;
use symphonia_core::packet::PacketRef;

use crate::codec::{EncodedPacket, FRAME_SAMPLES, MAX_PACKET_BYTES, SAMPLE_RATE};
use crate::file_player::{PacketSource, PlayerFailure};
use crate::ogg::OggOpus;
use crate::resample::RateConverter;

/// Largest file accepted (Discord's limit is 512 KB).
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;
/// Longest clip played (Discord's limit is 5.2 s).
pub const MAX_CLIP_MS: u64 = 6_000;
const MAX_PACKETS: usize = (MAX_CLIP_MS / 20) as usize;
const BITRATE: i32 = 96_000;
const STEREO_FRAME: usize = FRAME_SAMPLES * 2;
/// Samples per MPEG-1 Layer III frame, the largest an MP3 frame decodes to.
const MP3_FRAME: usize = 1152;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SoundError {
    /// The file exceeds [`MAX_INPUT_BYTES`].
    TooLarge,
    /// The audio exceeds [`MAX_CLIP_MS`].
    TooLong,
    /// Not MP3 or Ogg Opus (for example Ogg Vorbis), or an unusual layout.
    Unsupported,
    /// Nothing could be decoded.
    Damaged,
}

impl std::fmt::Display for SoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "the sound file is too large",
            Self::TooLong => "the sound is too long",
            Self::Unsupported => "the audio format of this sound is not supported",
            Self::Damaged => "the sound file is damaged",
        })
    }
}

impl std::error::Error for SoundError {}

/// A decoded sound: compact packet storage, played once from the start.
pub struct Clip {
    data: Vec<u8>,
    /// End offset of every packet in `data`.
    ends: Vec<u32>,
    next: usize,
}

impl Clip {
    fn new() -> Self {
        Self {
            data: Vec::new(),
            ends: Vec::new(),
            next: 0,
        }
    }

    fn push(&mut self, packet: &[u8]) -> Result<(), SoundError> {
        if self.ends.len() >= MAX_PACKETS {
            return Err(SoundError::TooLong);
        }
        self.data.extend_from_slice(packet);
        self.ends.push(self.data.len() as u32);
        Ok(())
    }

    pub fn len_ms(&self) -> u64 {
        self.ends.len() as u64 * 20
    }

    fn packet(&self, index: usize) -> Option<&[u8]> {
        let end = *self.ends.get(index)? as usize;
        let start = index.checked_sub(1).map_or(0, |i| self.ends[i] as usize);
        Some(&self.data[start..end])
    }
}

impl PacketSource for Clip {
    fn duration_ms(&self) -> Option<u64> {
        Some(self.len_ms())
    }

    fn next_packet(&mut self) -> Result<Option<EncodedPacket>, PlayerFailure> {
        let Some(bytes) = self.packet(self.next) else {
            return Ok(None);
        };
        let packet = EncodedPacket::from_slice(bytes).map_err(|_| PlayerFailure::Damaged)?;
        self.next += 1;
        Ok(Some(packet))
    }

    fn seek_ms(&mut self, ms: u64) -> Result<(), PlayerFailure> {
        self.next = ((ms / 20) as usize).min(self.ends.len());
        Ok(())
    }
}

/// Decodes a downloaded sound. Blocking and CPU-bound: call it on a worker thread.
pub fn decode(bytes: &[u8]) -> Result<Clip, SoundError> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(SoundError::TooLarge);
    }
    if bytes.starts_with(b"OggS") {
        return decode_ogg_opus(bytes);
    }
    let clip = catch_unwind(AssertUnwindSafe(|| decode_mp3(strip_id3(bytes))))
        .map_err(|_| SoundError::Damaged)??;
    if clip.ends.is_empty() {
        return Err(SoundError::Damaged);
    }
    Ok(clip)
}

fn decode_ogg_opus(bytes: &[u8]) -> Result<Clip, SoundError> {
    let mut ogg = OggOpus::open(Cursor::new(bytes)).map_err(|_| SoundError::Unsupported)?;
    let mut clip = Clip::new();
    while let Some(packet) = ogg.next_packet().map_err(|_| SoundError::Damaged)? {
        clip.push(packet.as_bytes())?;
    }
    if clip.ends.is_empty() {
        return Err(SoundError::Damaged);
    }
    Ok(clip)
}

/// The audio after a leading ID3v2 tag, which the MPEG reader expects to be gone.
fn strip_id3(bytes: &[u8]) -> &[u8] {
    if bytes.len() >= 10 && &bytes[..3] == b"ID3" {
        let size = bytes[6..10]
            .iter()
            .fold(0usize, |acc, &b| (acc << 7) | usize::from(b & 0x7F));
        let footer = if bytes[5] & 0x10 != 0 { 10 } else { 0 };
        return bytes.get(10 + size + footer..).unwrap_or(&[]);
    }
    bytes
}

fn decode_mp3(bytes: &[u8]) -> Result<Clip, SoundError> {
    let stream = MediaSourceStream::new(Box::new(Cursor::new(bytes)), Default::default());
    let mut reader = MpaReader::try_new(stream, FormatOptions::default())
        .map_err(|_| SoundError::Unsupported)?;
    let mut params = AudioCodecParameters::new();
    params.for_codec(CODEC_ID_MP3);
    let mut decoder = MpaDecoder::try_new(&params, &AudioDecoderOptions::default())
        .map_err(|_| SoundError::Unsupported)?;
    let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
        .map_err(|_| SoundError::Unsupported)?;
    encoder
        .set_bitrate(Bitrate::Bits(BITRATE))
        .map_err(|_| SoundError::Unsupported)?;
    let mut sink = Sink {
        clip: Clip::new(),
        encoder,
        pcm: Vec::with_capacity(STEREO_FRAME * 4),
        packet: vec![0; MAX_PACKET_BYTES],
    };
    let mut resampler: Option<(u32, RateConverter)> = None;
    let mut decoded = Vec::new();
    let mut failures = 0u32;
    while let Some(packet) = reader.next_packet().map_err(|_| SoundError::Damaged)? {
        let buffer = match decoder.decode_ref(&PacketRef::from(&packet)) {
            Ok(buffer) => buffer,
            Err(_) => {
                // A damaged frame is skipped; a file of little else is damaged.
                failures += 1;
                if failures > 50 {
                    return Err(SoundError::Damaged);
                }
                continue;
            }
        };
        let (rate, channels) = (buffer.spec().rate(), buffer.spec().channels().count());
        decoded.clear();
        buffer.copy_to_vec_interleaved::<f32>(&mut decoded);
        if channels == 1 {
            let mono = std::mem::take(&mut decoded);
            decoded.extend(mono.iter().flat_map(|&s| [s, s]));
        } else if channels != 2 {
            return Err(SoundError::Unsupported);
        }
        if resampler.as_ref().is_none_or(|(known, _)| *known != rate) {
            let converter = RateConverter::new(rate, SAMPLE_RATE, 2, MP3_FRAME)
                .map_err(|_| SoundError::Unsupported)?;
            resampler = Some((rate, converter));
        }
        let (_, converter) = resampler.as_mut().ok_or(SoundError::Unsupported)?;
        let mut result = Ok(());
        converter.process(&decoded, &mut |block| {
            if result.is_ok() {
                result = sink.feed(block);
            }
        });
        result?;
    }
    // Push the converter's retained tail through with silence, then pad the last packet.
    if let Some((_, converter)) = resampler.as_mut() {
        let mut result = Ok(());
        converter.process(&vec![0.0; MP3_FRAME * 2], &mut |block| {
            if result.is_ok() {
                result = sink.feed(block);
            }
        });
        result?;
    }
    sink.finish()
}

struct Sink {
    clip: Clip,
    encoder: Encoder,
    /// Interleaved stereo at 48 kHz not yet encoded.
    pcm: Vec<f32>,
    packet: Vec<u8>,
}

impl Sink {
    fn feed(&mut self, block: &[f32]) -> Result<(), SoundError> {
        self.pcm.extend_from_slice(block);
        while self.pcm.len() >= STEREO_FRAME {
            self.encode()?;
        }
        Ok(())
    }

    fn encode(&mut self) -> Result<(), SoundError> {
        let len = self
            .encoder
            .encode_float(&self.pcm[..STEREO_FRAME], &mut self.packet)
            .map_err(|_| SoundError::Damaged)?;
        self.pcm.drain(..STEREO_FRAME);
        self.clip.push(&self.packet[..len])
    }

    fn finish(mut self) -> Result<Clip, SoundError> {
        if !self.pcm.is_empty() {
            self.pcm.resize(STEREO_FRAME, 0.0);
            self.encode()?;
        }
        Ok(self.clip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ogg::write_ogg_opus;

    /// Silent MPEG-1 Layer III frames: 128 kbit/s, 44.1 kHz, stereo, 417 bytes each.
    fn silent_mp3(frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..frames {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
            out.extend(std::iter::repeat_n(0u8, 413));
        }
        out
    }

    #[test]
    fn mp3_becomes_20_ms_packets_of_the_same_length_and_id3_is_skipped() {
        let mut file = b"ID3\x03\x00\x00\x00\x00\x00\x04abcd".to_vec();
        file.extend(silent_mp3(40));
        let mut clip = decode(&file).unwrap();
        // 40 frames of 1152 samples at 44.1 kHz is about 1045 ms.
        assert!(clip.len_ms().abs_diff(1045) <= 80, "{}", clip.len_ms());
        let mut count = 0;
        while clip.next_packet().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count as u64 * 20, clip.len_ms());
        clip.seek_ms(40).unwrap();
        assert!(clip.next_packet().unwrap().is_some());
    }

    #[test]
    fn ogg_opus_passes_through_and_long_oversized_or_foreign_input_is_refused() {
        let packets: Vec<Vec<u8>> = (0..10).map(|_| vec![0xF8]).collect();
        let ogg = write_ogg_opus(&packets).unwrap();
        assert_eq!(decode(&ogg).unwrap().len_ms(), 200);
        let long: Vec<Vec<u8>> = (0..MAX_PACKETS + 5).map(|_| vec![0xF8]).collect();
        assert_eq!(
            decode(&write_ogg_opus(&long).unwrap()).err(),
            Some(SoundError::TooLong)
        );
        assert_eq!(
            decode(&silent_mp3(400)).err(),
            Some(SoundError::TooLong),
            "about 10 s"
        );
        assert_eq!(
            decode(&vec![0; MAX_INPUT_BYTES + 1]).err(),
            Some(SoundError::TooLarge)
        );
        // Ogg that is not Opus and plain garbage.
        let vorbis = [b"OggS".as_slice(), &[0u8; 60]].concat();
        assert!(decode(&vorbis).is_err());
        assert!(decode(b"definitely not audio, just text").is_err());
        assert!(decode(&[]).is_err());
    }
}
