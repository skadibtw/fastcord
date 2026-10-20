//! AAC-LC audio of a container as 20 ms Opus packets for the file player (issue #116; ADR 0008).
//!
//! The engine's remote-speaker path takes Opus, so AAC is decoded with the pure-Rust
//! `symphonia-codec-aac` (AAC-LC, mono or stereo, no SBR), converted to 48 kHz stereo and
//! encoded again with libopus at [`BITRATE`]. All of it runs on the player's control thread
//! in front of the engine; audio callbacks are untouched. Memory is one AAC frame of PCM
//! plus one Opus frame: the caller pops every ready packet before pushing the next frame.
//!
//! A frame that fails to decode (damaged file) becomes silence of the same length so the
//! clock keeps its place; a decoder that panics on hostile input is dropped and rebuilt.

use std::panic::{AssertUnwindSafe, catch_unwind};

use opus2::{Application, Bitrate, Channels, Encoder};
use symphonia_codec_aac::AacDecoder;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

use crate::codec::{EncodedPacket, FRAME_SAMPLES, MAX_PACKET_BYTES, SAMPLE_RATE};
use crate::resample::RateConverter;

/// Samples per AAC-LC frame.
const AAC_FRAME: usize = 1024;
/// Opus bitrate of the re-encoded audio: transparent enough for a message attachment.
const BITRATE: i32 = 128_000;
const STEREO_FRAME: usize = FRAME_SAMPLES * 2;

/// Why an AAC track cannot be played.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AacUnsupported;

pub struct AacTranscoder {
    config: Box<[u8]>,
    rate: u32,
    decoder: Option<AacDecoder>,
    resampler: RateConverter,
    encoder: Encoder,
    /// Decoded interleaved stereo at 48 kHz that has not been encoded yet.
    pcm: Vec<f32>,
    decoded: Vec<f32>,
    packet: Vec<u8>,
}

fn open_decoder(config: &[u8]) -> Result<AacDecoder, AacUnsupported> {
    let mut params = AudioCodecParameters::new();
    params
        .for_codec(CODEC_ID_AAC)
        .with_extra_data(config.into());
    catch_unwind(|| AacDecoder::try_new(&params, &AudioDecoderOptions::default()))
        .map_err(|_| AacUnsupported)?
        .map_err(|_| AacUnsupported)
}

impl AacTranscoder {
    /// `config` is the AudioSpecificConfig of the track, `rate` its sample rate.
    pub fn new(config: &[u8], rate: u32) -> Result<Self, AacUnsupported> {
        let decoder = open_decoder(config)?;
        let resampler =
            RateConverter::new(rate, SAMPLE_RATE, 2, AAC_FRAME).map_err(|_| AacUnsupported)?;
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Audio)
            .map_err(|_| AacUnsupported)?;
        encoder
            .set_bitrate(Bitrate::Bits(BITRATE))
            .map_err(|_| AacUnsupported)?;
        Ok(Self {
            config: config.into(),
            rate,
            decoder: Some(decoder),
            resampler,
            encoder,
            pcm: Vec::with_capacity(STEREO_FRAME * 8),
            decoded: Vec::with_capacity(AAC_FRAME * 2),
            packet: vec![0; MAX_PACKET_BYTES],
        })
    }

    /// Forgets the stream (seek): pending audio and decoder history go.
    pub fn reset(&mut self) {
        self.pcm.clear();
        self.resampler.reset();
        if let Some(decoder) = &mut self.decoder {
            decoder.reset();
        }
        let _ = self.encoder.reset_state();
    }

    /// Decodes one AAC frame and queues its audio; with `discard` the audio is dropped
    /// (a seek decodes the frame before its target to prime the overlap-add).
    /// Returns whether the frame decoded; a damaged one queues silence instead.
    pub fn push(&mut self, frame: &[u8], discard: bool) -> bool {
        self.decoded.clear();
        let ok = self.decode(frame);
        if discard {
            self.decoded.clear();
            return ok;
        }
        if !ok {
            let silent = AAC_FRAME * SAMPLE_RATE as usize / self.rate as usize;
            self.pcm.resize(self.pcm.len() + silent * 2, 0.0);
            return false;
        }
        let pcm = &mut self.pcm;
        self.resampler
            .process(&self.decoded, &mut |block| pcm.extend_from_slice(block));
        true
    }

    /// Fills `self.decoded` with interleaved stereo at the track's rate.
    fn decode(&mut self, frame: &[u8]) -> bool {
        if self.decoder.is_none() {
            self.decoder = open_decoder(&self.config).ok();
        }
        let Some(decoder) = self.decoder.as_mut() else {
            return false;
        };
        let packet = PacketRef::new(0, Timestamp::new(0), Duration::new(AAC_FRAME as u64), frame);
        let decoded = &mut self.decoded;
        let result = catch_unwind(AssertUnwindSafe(|| {
            let buffer = decoder.decode_ref(&packet).ok()?;
            let channels = buffer.spec().channels().count();
            buffer.copy_to_vec_interleaved::<f32>(decoded);
            Some(channels)
        }));
        match result {
            Ok(Some(2)) => true,
            Ok(Some(1)) => {
                let mono = std::mem::take(decoded);
                decoded.extend(mono.iter().flat_map(|&s| [s, s]));
                true
            }
            Ok(_) => false,
            // The decoder's state is unknown after a panic.
            Err(_) => {
                self.decoder = None;
                false
            }
        }
    }

    /// The next 20 ms packet, when enough audio is queued.
    pub fn pop(&mut self) -> Option<EncodedPacket> {
        if self.pcm.len() < STEREO_FRAME {
            return None;
        }
        let len = self
            .encoder
            .encode_float(&self.pcm[..STEREO_FRAME], &mut self.packet)
            .ok();
        self.pcm.drain(..STEREO_FRAME);
        EncodedPacket::from_slice(&self.packet[..len?]).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_frames_never_panic_and_keep_the_clock_with_silence() {
        let mut aac = AacTranscoder::new(&[0x12, 0x10], 44_100).unwrap();
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut packets = 0;
        for _ in 0..300 {
            let len = (next() % 400) as usize;
            let frame: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            aac.push(&frame, false);
            while aac.pop().is_some() {
                packets += 1;
            }
        }
        // Every frame, decoded or not, adds 1024 samples at 44.1 kHz: about 23.2 ms.
        let expected: usize = 300 * 1024 * 48_000 / 44_100 / 960;
        assert!(expected.abs_diff(packets) <= 8, "{packets}");
    }

    #[test]
    fn unsupported_configurations_are_refused() {
        // SBR object type; HE-AAC is not decoded.
        assert!(AacTranscoder::new(&[0x2A, 0x10], 44_100).is_err());
        assert!(AacTranscoder::new(&[], 44_100).is_err());
    }
}
