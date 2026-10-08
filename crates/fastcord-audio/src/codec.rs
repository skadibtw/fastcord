//! Opus voice codec (libopus via `opus2`, bundled and statically linked).
//!
//! Discord voice is 48 kHz stereo Opus in 20 ms frames (960 samples per
//! channel). The microphone is mono, so the encoder receives the mono signal in
//! both channels and is forced to code it as mono, spending every bit on the
//! one real channel; stereo decoders play such packets on both channels.
//! Buffers are preallocated: encoding and decoding do not allocate.

use opus2::{Application, Bitrate, Channels, Decoder, Encoder, ErrorCode, Signal};

/// Opus clock and the engine's internal rate.
pub const SAMPLE_RATE: u32 = 48_000;
/// Interleaved channel count on the wire.
pub const CHANNELS: usize = 2;
/// Samples per channel in one 20 ms frame (also the RTP timestamp step).
pub const FRAME_SAMPLES: usize = 960;
/// Longest Opus packet duration (120 ms), per channel.
pub const MAX_FRAME_SAMPLES: usize = 5_760;
/// Largest encoded packet accepted. One 20 ms Opus frame is at most 1275
/// bytes; multi-frame packets are bounded by what fits a non-fragmented UDP
/// datagram, so anything larger is rejected rather than buffered.
pub const MAX_PACKET_BYTES: usize = 1_500;
/// Default microphone bitrate (SPEC §6.3).
pub const DEFAULT_BITRATE: u32 = 64_000;
/// Initial packet-loss expectation; non-zero so in-band FEC is produced from
/// the first packet. The media session adapts it from observed loss.
pub const INITIAL_EXPECTED_LOSS_PERCENT: u8 = 5;

/// Largest single 20 ms frame libopus can produce.
const MAX_FRAME_BYTES: usize = 1_275;

/// An Opus error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecError {
    /// libopus rejected a packet or call.
    Opus(ErrorCode),
    /// An empty packet was offered for decoding (loss uses
    /// [`VoiceDecoder::conceal`] or [`VoiceDecoder::recover`] instead).
    EmptyPacket,
    /// A packet longer than [`MAX_PACKET_BYTES`].
    PacketTooLarge,
    /// A concealment duration that is not a positive multiple of 2.5 ms up to
    /// 120 ms.
    InvalidDuration,
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opus(code) => write!(f, "Opus error: {}", code.description()),
            Self::EmptyPacket => f.write_str("empty Opus packet"),
            Self::PacketTooLarge => f.write_str("Opus packet too large"),
            Self::InvalidDuration => f.write_str("invalid Opus frame duration"),
        }
    }
}

impl std::error::Error for CodecError {}

impl From<opus2::Error> for CodecError {
    fn from(error: opus2::Error) -> Self {
        Self::Opus(error.code())
    }
}

/// An encoded Opus packet in fixed inline storage, so packets move through
/// bounded channels without heap allocation.
#[derive(Clone)]
pub struct EncodedPacket {
    len: u16,
    bytes: [u8; MAX_PACKET_BYTES],
}

impl EncodedPacket {
    /// Copies `bytes` into a packet.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, CodecError> {
        if bytes.len() > MAX_PACKET_BYTES {
            return Err(CodecError::PacketTooLarge);
        }
        let mut packet = Self::empty();
        packet.bytes[..bytes.len()].copy_from_slice(bytes);
        packet.len = bytes.len() as u16;
        Ok(packet)
    }

    pub(crate) fn empty() -> Self {
        Self {
            len: 0,
            bytes: [0; MAX_PACKET_BYTES],
        }
    }

    /// The packet's bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

impl PartialEq for EncodedPacket {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for EncodedPacket {}

impl std::fmt::Debug for EncodedPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncodedPacket")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

/// Microphone encoder: mono 48 kHz input, stereo Opus stream.
pub struct VoiceEncoder {
    encoder: Encoder,
    interleaved: Box<[f32; FRAME_SAMPLES * CHANNELS]>,
}

impl VoiceEncoder {
    /// Creates a VoIP-tuned encoder at `bitrate` bits/s with in-band FEC.
    pub fn new(bitrate: u32) -> Result<Self, CodecError> {
        let mut encoder = Encoder::new(SAMPLE_RATE, Channels::Stereo, Application::Voip)?;
        encoder.set_signal(Signal::Voice)?;
        encoder.set_force_channels(Some(Channels::Mono))?;
        encoder.set_vbr(true)?;
        encoder.set_inband_fec(true)?;
        let mut this = Self {
            encoder,
            interleaved: Box::new([0.0; FRAME_SAMPLES * CHANNELS]),
        };
        this.set_bitrate(bitrate)?;
        this.set_expected_loss(INITIAL_EXPECTED_LOSS_PERCENT)?;
        Ok(this)
    }

    /// Sets the target bitrate, clamped to Opus's 6–510 kbit/s range.
    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), CodecError> {
        let bits = bitrate.clamp(6_000, 510_000) as i32;
        self.encoder.set_bitrate(Bitrate::Bits(bits))?;
        Ok(())
    }

    /// Sets the expected packet loss (0–100 %), which scales FEC redundancy.
    pub fn set_expected_loss(&mut self, percent: u8) -> Result<(), CodecError> {
        self.encoder
            .set_packet_loss_perc(i32::from(percent.min(100)))?;
        Ok(())
    }

    /// Encodes one 20 ms mono frame into `out`.
    pub fn encode_mono(
        &mut self,
        mono: &[f32; FRAME_SAMPLES],
        out: &mut EncodedPacket,
    ) -> Result<(), CodecError> {
        for (frame, &sample) in self
            .interleaved
            .as_chunks_mut::<CHANNELS>()
            .0
            .iter_mut()
            .zip(mono)
        {
            frame.fill(sample);
        }
        let len = self
            .encoder
            .encode_float(&self.interleaved[..], &mut out.bytes[..MAX_FRAME_BYTES])?;
        out.len = len as u16;
        Ok(())
    }
}

/// Decoder for one incoming stream: stereo 48 kHz output.
pub struct VoiceDecoder {
    decoder: Decoder,
}

impl VoiceDecoder {
    pub fn new() -> Result<Self, CodecError> {
        Ok(Self {
            decoder: Decoder::new(SAMPLE_RATE, Channels::Stereo)?,
        })
    }

    /// Decodes `packet` into interleaved stereo `out`, which must hold
    /// [`MAX_FRAME_SAMPLES`] frames. Returns frames decoded. Malformed packets
    /// are errors, never panics.
    pub fn decode(&mut self, packet: &[u8], out: &mut [f32]) -> Result<usize, CodecError> {
        if packet.is_empty() {
            return Err(CodecError::EmptyPacket);
        }
        if packet.len() > MAX_PACKET_BYTES {
            return Err(CodecError::PacketTooLarge);
        }
        let out = Self::output(out, MAX_FRAME_SAMPLES)?;
        Ok(self.decoder.decode_float(packet, out, false)?)
    }

    /// Reconstructs a lost frame of `frames` samples per channel from the
    /// in-band FEC data carried by the packet that followed it.
    pub fn recover(
        &mut self,
        next_packet: &[u8],
        frames: usize,
        out: &mut [f32],
    ) -> Result<usize, CodecError> {
        if next_packet.is_empty() {
            return Err(CodecError::EmptyPacket);
        }
        if next_packet.len() > MAX_PACKET_BYTES {
            return Err(CodecError::PacketTooLarge);
        }
        let out = Self::output(out, Self::loss_duration(frames)?)?;
        Ok(self.decoder.decode_float(next_packet, out, true)?)
    }

    /// Synthesizes `frames` samples per channel of packet-loss concealment.
    pub fn conceal(&mut self, frames: usize, out: &mut [f32]) -> Result<usize, CodecError> {
        let out = Self::output(out, Self::loss_duration(frames)?)?;
        Ok(self.decoder.decode_float(&[], out, false)?)
    }

    /// Forgets the stream history (new talkspurt source or SSRC reuse).
    pub fn reset(&mut self) -> Result<(), CodecError> {
        self.decoder.reset_state()?;
        Ok(())
    }

    fn loss_duration(frames: usize) -> Result<usize, CodecError> {
        // Opus durations are multiples of 2.5 ms (120 samples at 48 kHz).
        if frames == 0 || frames > MAX_FRAME_SAMPLES || !frames.is_multiple_of(120) {
            return Err(CodecError::InvalidDuration);
        }
        Ok(frames)
    }

    fn output(out: &mut [f32], frames: usize) -> Result<&mut [f32], CodecError> {
        out.get_mut(..frames * CHANNELS)
            .ok_or(CodecError::Opus(ErrorCode::BufferTooSmall))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resample::tests::{sine, tone_level};

    fn encode_tone(frames: usize, frequency: f32) -> (Vec<EncodedPacket>, Vec<f32>) {
        let mut encoder = VoiceEncoder::new(DEFAULT_BITRATE).unwrap();
        let signal = sine(SAMPLE_RATE, frequency, 0.5, frames * FRAME_SAMPLES);
        let packets = signal
            .as_chunks::<FRAME_SAMPLES>()
            .0
            .iter()
            .map(|chunk| {
                let mut packet = EncodedPacket::empty();
                encoder.encode_mono(chunk, &mut packet).unwrap();
                packet
            })
            .collect();
        (packets, signal)
    }

    fn left(stereo: &[f32]) -> Vec<f32> {
        stereo.iter().step_by(2).copied().collect()
    }

    #[test]
    fn encode_decode_round_trip_preserves_a_tone_on_both_channels() {
        let (packets, _) = encode_tone(50, 440.0);
        let mut decoder = VoiceDecoder::new().unwrap();
        let mut decoded = Vec::new();
        let mut out = vec![0.0; MAX_FRAME_SAMPLES * CHANNELS];
        for packet in &packets {
            assert!(packet.as_bytes().len() <= MAX_FRAME_BYTES);
            assert_eq!(
                opus2::packet::get_nb_channels(packet.as_bytes()).unwrap(),
                Channels::Mono
            );
            let frames = decoder.decode(packet.as_bytes(), &mut out).unwrap();
            assert_eq!(frames, FRAME_SAMPLES);
            decoded.extend_from_slice(&out[..frames * CHANNELS]);
        }
        let steady = &decoded[decoded.len() / 2..];
        let l = left(steady);
        let r: Vec<f32> = steady.iter().skip(1).step_by(2).copied().collect();
        assert!((tone_level(&l, SAMPLE_RATE, 440.0) - 0.5).abs() < 0.05);
        assert_eq!(
            l, r,
            "mono-coded stream decodes identically on both channels"
        );
        // ~64 kbit/s target: 20 ms frames average around 160 bytes.
        let average = packets.iter().map(|p| p.as_bytes().len()).sum::<usize>() / packets.len();
        assert!(
            (80..=260).contains(&average),
            "average packet {average} bytes"
        );
    }

    #[test]
    fn fec_recovers_a_lost_frame_better_than_concealment() {
        let (packets, signal) = encode_tone(40, 660.0);
        let lost = 30;
        let reference = &signal[lost * FRAME_SAMPLES..(lost + 1) * FRAME_SAMPLES];
        let mut out = vec![0.0; MAX_FRAME_SAMPLES * CHANNELS];

        let mut run = |use_fec: bool| {
            let mut decoder = VoiceDecoder::new().unwrap();
            for packet in &packets[..lost] {
                decoder.decode(packet.as_bytes(), &mut out).unwrap();
            }
            let frames = if use_fec {
                decoder
                    .recover(packets[lost + 1].as_bytes(), FRAME_SAMPLES, &mut out)
                    .unwrap()
            } else {
                decoder.conceal(FRAME_SAMPLES, &mut out).unwrap()
            };
            assert_eq!(frames, FRAME_SAMPLES);
            left(&out[..frames * CHANNELS])
        };
        let recovered = run(true);
        let concealed = run(false);
        let level = |x: &[f32]| tone_level(x, SAMPLE_RATE, 660.0);
        assert!(level(&recovered) > 0.3, "FEC level {}", level(&recovered));
        // Encoder lookahead shifts the decoded phase, so compare the tone
        // level rather than samples.
        assert!(level(&recovered) >= level(&concealed) * 0.9);
        assert!((level(reference) - 0.5).abs() < 0.01);
    }

    #[test]
    fn malformed_and_oversized_input_is_rejected_without_panicking() {
        let mut decoder = VoiceDecoder::new().unwrap();
        let mut out = vec![0.0; MAX_FRAME_SAMPLES * CHANNELS];
        assert_eq!(decoder.decode(&[], &mut out), Err(CodecError::EmptyPacket));
        assert_eq!(
            decoder.decode(&[0; MAX_PACKET_BYTES + 1], &mut out),
            Err(CodecError::PacketTooLarge)
        );
        // TOC code 3 with a frame count of zero is invalid per RFC 6716.
        assert_eq!(
            decoder.decode(&[0x03, 0x00], &mut out),
            Err(CodecError::Opus(ErrorCode::InvalidPacket))
        );
        let mut small = [0.0; 10];
        assert_eq!(
            decoder.decode(&[0xF8, 0xFF, 0xFE], &mut small),
            Err(CodecError::Opus(ErrorCode::BufferTooSmall))
        );
        assert_eq!(
            decoder.conceal(100, &mut out),
            Err(CodecError::InvalidDuration)
        );
        assert_eq!(
            decoder.conceal(0, &mut out),
            Err(CodecError::InvalidDuration)
        );
        assert_eq!(
            decoder.conceal(MAX_FRAME_SAMPLES + 120, &mut out),
            Err(CodecError::InvalidDuration)
        );
        // The decoder remains usable after rejecting input.
        let (packets, _) = encode_tone(1, 440.0);
        assert_eq!(
            decoder.decode(packets[0].as_bytes(), &mut out),
            Ok(FRAME_SAMPLES)
        );
    }

    #[test]
    fn packet_storage_is_bounded() {
        assert!(EncodedPacket::from_slice(&[1; MAX_PACKET_BYTES]).is_ok());
        assert_eq!(
            EncodedPacket::from_slice(&[1; MAX_PACKET_BYTES + 1]),
            Err(CodecError::PacketTooLarge)
        );
        let packet = EncodedPacket::from_slice(&[1, 2, 3]).unwrap();
        assert_eq!(packet.as_bytes(), &[1, 2, 3]);
        assert_eq!(format!("{packet:?}"), "EncodedPacket { len: 3, .. }");
    }

    #[test]
    fn bitrate_and_loss_settings_are_clamped() {
        let mut encoder = VoiceEncoder::new(1).unwrap();
        assert_eq!(encoder.encoder.get_bitrate().unwrap(), Bitrate::Bits(6_000));
        encoder.set_bitrate(10_000_000).unwrap();
        assert_eq!(
            encoder.encoder.get_bitrate().unwrap(),
            Bitrate::Bits(510_000)
        );
        encoder.set_expected_loss(250).unwrap();
        assert_eq!(encoder.encoder.get_packet_loss_perc().unwrap(), 100);
        assert!(encoder.encoder.get_inband_fec().unwrap());
    }
}
