//! Bounded MP4 (ISO BMFF, QuickTime) and WebM/Matroska demuxing for inline attachment
//! playback (issue #116; ADR 0007).
//!
//! [`open`] indexes a whole file once, without reading its media data: every sample's
//! file offset, size, presentation time and keyframe flag for the first supported video
//! track (H.264 or AV1) and the first audio track. [`read_sample`] then reads one sample
//! on demand, so memory is the index (at most [`MAX_SAMPLES`] x 24 bytes) plus the sample
//! being worked on, whatever the file length. Metadata is read from the file in bounded
//! pieces ([`MAX_METADATA_BYTES`]); nothing is trusted: sizes are checked against the file
//! length and the remaining parent box before use, and malformed or hostile input yields an
//! error, never a panic or an unbounded allocation.
//!
//! Unsupported on purpose: fragmented MP4, lacing and content encodings in Matroska,
//! H.265/VP8/VP9/MPEG video, and audio other than Opus and AAC-LC (reported through
//! [`Media::audio_skipped`] so the caller can play the video silently). This module only
//! frames data; decoding is [`crate::VideoDecoder`] and the audio engine.

use std::io::{Read, Seek, SeekFrom};

mod matroska;
mod mp4;

/// Most samples (video plus audio) one file may index: about 9.6 MB of index at most.
pub const MAX_SAMPLES: usize = 400_000;
/// Largest single sample accepted; equals [`crate::h264::MAX_ACCESS_UNIT_BYTES`].
pub const MAX_SAMPLE_BYTES: u32 = 8 << 20;
/// Largest metadata block (MP4 `moov`, Matroska `Tracks`/`Info`) held in memory.
pub const MAX_METADATA_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerError {
    /// Reading failed.
    Io,
    /// The file is damaged, truncated or contradicts itself.
    Malformed,
    /// More than [`MAX_SAMPLES`] samples, or a sample or metadata block over its limit.
    TooLarge,
    /// A fragmented MP4 (`mvex`/`moof`).
    Fragmented,
    /// A video track whose codec is neither H.264 nor AV1, or that is encrypted or
    /// otherwise transformed.
    UnsupportedVideo,
    /// Neither a playable video track nor an Opus audio track.
    NoPlayableTrack,
    /// Not an MP4 or Matroska file.
    UnknownFormat,
}

impl std::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Io => "the file could not be read",
            Self::Malformed => "the media file is damaged",
            Self::TooLarge => "the media file has too many or too large samples",
            Self::Fragmented => "fragmented MP4 is not supported",
            Self::UnsupportedVideo => "the video codec is not supported",
            Self::NoPlayableTrack => "the file has no playable track",
            Self::UnknownFormat => "the file is not an MP4 or WebM",
        })
    }
}

impl std::error::Error for ContainerError {}

impl From<std::io::Error> for ContainerError {
    fn from(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Malformed
        } else {
            Self::Io
        }
    }
}

/// One coded frame or audio packet in the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    pub offset: u64,
    pub size: u32,
    /// Presentation time in milliseconds from the start of the media.
    pub pts_ms: u32,
    pub key: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoCodecId {
    H264,
    Av1,
}

/// An audio format that cannot be played; the video then plays without sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSkipped {
    Aac,
    Other,
}

#[derive(Clone, Debug)]
pub struct VideoTrack {
    pub codec: VideoCodecId,
    pub width: u32,
    pub height: u32,
    /// Samples in decoding order.
    pub samples: Vec<Sample>,
    /// H.264: length-prefix size (1-4) of the stored NAL units. Zero for AV1.
    nal_length_size: usize,
    /// H.264: SPS and PPS in Annex B. AV1: the configuration OBUs (sequence header).
    config: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct AudioTrack {
    pub codec: AudioCodecId,
    pub channels: u8,
    /// Sample rate of the coded audio (48 000 for Opus).
    pub rate: u32,
    /// AAC: the AudioSpecificConfig. Empty for Opus.
    pub config: Vec<u8>,
    /// Opus packets or AAC frames in playing order.
    pub samples: Vec<Sample>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioCodecId {
    Opus,
    Aac,
}

/// Sample rate and channel count of an AAC-LC AudioSpecificConfig with 1024-sample frames
/// and one or two channels, the only AAC the player decodes; `None` for anything else.
pub fn aac_lc_info(asc: &[u8]) -> Option<(u32, u8)> {
    const RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];
    let mut bits = asc
        .iter()
        .flat_map(|b| (0..8).rev().map(move |i| (b >> i) & 1));
    let mut take = |n: u32| -> Option<u32> {
        (0..n).try_fold(0u32, |acc, _| Some((acc << 1) | u32::from(bits.next()?)))
    };
    if take(5)? != 2 {
        return None;
    }
    let rate = match take(4)? {
        index @ 0..=12 => RATES[index as usize],
        15 => take(24)?,
        _ => return None,
    };
    let channels = take(4)?;
    let frame_length_flag = take(1)?;
    (rate != 0 && matches!(channels, 1 | 2) && frame_length_flag == 0)
        .then_some((rate, channels as u8))
}

#[derive(Clone, Debug)]
pub struct Media {
    pub duration_ms: u64,
    pub video: Option<VideoTrack>,
    pub audio: Option<AudioTrack>,
    /// Set when the file has audio this client cannot decode.
    pub audio_skipped: Option<AudioSkipped>,
}

/// The container format of a file's first bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Mp4,
    Matroska,
}

pub fn sniff(head: &[u8]) -> Option<Format> {
    if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return Some(Format::Matroska);
    }
    let kind = head.get(4..8)?;
    [&b"ftyp"[..], b"moov", b"mdat", b"free", b"wide", b"skip"]
        .contains(&kind)
        .then_some(Format::Mp4)
}

/// Indexes `reader`. Blocks on file reads: call it off the UI thread.
pub fn open<R: Read + Seek>(reader: &mut R) -> Result<Media, ContainerError> {
    let len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 8];
    let got = read_up_to(reader, &mut head)?;
    let media = match sniff(&head[..got]).ok_or(ContainerError::UnknownFormat)? {
        Format::Mp4 => mp4::open(reader, len)?,
        Format::Matroska => matroska::open(reader, len)?,
    };
    if media.video.is_none() && media.audio.is_none() {
        return Err(ContainerError::NoPlayableTrack);
    }
    Ok(media)
}

/// Reads as many of `buf` as the file has; a short count means the end of the file.
fn read_up_to<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize, ContainerError> {
    let mut done = 0;
    while done < buf.len() {
        match reader.read(&mut buf[done..]) {
            Ok(0) => break,
            Ok(n) => done += n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(done)
}

/// Reads the bytes of `sample` into `out`, replacing its contents.
pub fn read_sample<R: Read + Seek>(
    reader: &mut R,
    sample: &Sample,
    out: &mut Vec<u8>,
) -> Result<(), ContainerError> {
    if sample.size > MAX_SAMPLE_BYTES {
        return Err(ContainerError::TooLarge);
    }
    reader.seek(SeekFrom::Start(sample.offset))?;
    out.clear();
    out.resize(sample.size as usize, 0);
    reader.read_exact(out)?;
    Ok(())
}

impl VideoTrack {
    /// The access unit a decoder takes for the stored `sample` bytes: Annex B for H.264
    /// (parameter sets in front of a keyframe), the sized OBUs for AV1 (the
    /// configuration's sequence header in front of a keyframe).
    pub fn access_unit(
        &self,
        stored: &[u8],
        key: bool,
        out: &mut Vec<u8>,
    ) -> Result<(), ContainerError> {
        out.clear();
        if key {
            out.extend_from_slice(&self.config);
        }
        match self.codec {
            VideoCodecId::Av1 => out.extend_from_slice(stored),
            VideoCodecId::H264 => {
                let mut rest = stored;
                while !rest.is_empty() {
                    let size = self.nal_length_size;
                    let length = rest
                        .get(..size)
                        .ok_or(ContainerError::Malformed)?
                        .iter()
                        .fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
                    rest = &rest[size..];
                    let nal = rest.get(..length).ok_or(ContainerError::Malformed)?;
                    if nal.is_empty() {
                        return Err(ContainerError::Malformed);
                    }
                    out.extend_from_slice(&[0, 0, 0, 1]);
                    out.extend_from_slice(nal);
                    rest = &rest[length..];
                }
            }
        }
        Ok(())
    }

    /// Index of the keyframe to start decoding from to show the picture at `ms`: the last
    /// keyframe presented at or before it, or the first sample.
    pub fn keyframe_for(&self, ms: u64) -> usize {
        self.samples
            .iter()
            .rposition(|s| s.key && u64::from(s.pts_ms) <= ms)
            .unwrap_or(0)
    }
}

impl AudioTrack {
    /// Index of the first packet that starts at or after `ms`.
    pub fn index_at(&self, ms: u64) -> usize {
        self.samples.partition_point(|s| u64::from(s.pts_ms) < ms)
    }
}

/// Builds the Annex B parameter sets and NAL length size from an `avcC` record.
fn parse_avcc(record: &[u8]) -> Result<(Vec<u8>, usize), ContainerError> {
    if record.len() < 7 || record[0] != 1 {
        return Err(ContainerError::Malformed);
    }
    let length_size = usize::from(record[4] & 3) + 1;
    let mut config = Vec::new();
    let mut rest = &record[5..];
    let sets = |rest: &mut &[u8], count: usize, config: &mut Vec<u8>| {
        for _ in 0..count {
            let size = rest.get(..2).ok_or(ContainerError::Malformed)?;
            let size = usize::from(u16::from_be_bytes([size[0], size[1]]));
            let nal = rest.get(2..2 + size).ok_or(ContainerError::Malformed)?;
            if nal.is_empty() {
                return Err(ContainerError::Malformed);
            }
            config.extend_from_slice(&[0, 0, 0, 1]);
            config.extend_from_slice(nal);
            *rest = &rest[2 + size..];
        }
        Ok(())
    };
    let sps = usize::from(rest[0] & 0x1f);
    rest = &rest[1..];
    sets(&mut rest, sps, &mut config)?;
    let pps = usize::from(*rest.first().ok_or(ContainerError::Malformed)?);
    rest = &rest[1..];
    sets(&mut rest, pps, &mut config)?;
    if sps == 0 || pps == 0 {
        return Err(ContainerError::Malformed);
    }
    Ok((config, length_size))
}

/// Checks the index the demuxers built, in one place.
fn finish_samples(samples: &[Sample], file_len: u64) -> Result<(), ContainerError> {
    for sample in samples {
        if sample.size > MAX_SAMPLE_BYTES {
            return Err(ContainerError::TooLarge);
        }
        let end = sample
            .offset
            .checked_add(u64::from(sample.size))
            .ok_or(ContainerError::Malformed)?;
        if end > file_len {
            return Err(ContainerError::Malformed);
        }
    }
    Ok(())
}

fn ms_from(ticks: i128, per_second: i128) -> u32 {
    if per_second <= 0 {
        return 0;
    }
    u32::try_from((ticks.max(0) * 1000 / per_second).min(i128::from(u32::MAX))).unwrap_or(0)
}

#[cfg(test)]
mod tests;
