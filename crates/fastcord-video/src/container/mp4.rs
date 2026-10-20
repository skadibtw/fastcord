//! ISO BMFF (MP4, M4V, MOV) indexing. Only `moov` is read into memory; sample tables are
//! walked in place over that buffer.

use std::io::{Read, Seek, SeekFrom};

use super::{
    AudioSkipped, AudioTrack, ContainerError, MAX_METADATA_BYTES, MAX_SAMPLES, Media, Sample,
    VideoCodecId, VideoTrack, finish_samples, ms_from, parse_avcc, read_up_to,
};

const MAX_TOP_LEVEL_BOXES: usize = 4096;

type Result<T> = std::result::Result<T, ContainerError>;

/// The boxes of one level of a buffer: `(type, payload)`.
struct Boxes<'a>(&'a [u8]);

impl<'a> Iterator for Boxes<'a> {
    type Item = Result<([u8; 4], &'a [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let item = (|| {
            let head = self.0.get(..8).ok_or(ContainerError::Malformed)?;
            let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
            let kind = [head[4], head[5], head[6], head[7]];
            let (header, size) = match size32 {
                0 => (8, self.0.len() as u64),
                1 => {
                    let wide = self.0.get(8..16).ok_or(ContainerError::Malformed)?;
                    (16, u64::from_be_bytes(wide.try_into().unwrap()))
                }
                n => (8, u64::from(n)),
            };
            if size < header || size > self.0.len() as u64 {
                return Err(ContainerError::Malformed);
            }
            let (this, rest) = self.0.split_at(size as usize);
            self.0 = rest;
            Ok((kind, &this[header as usize..]))
        })();
        if item.is_err() {
            self.0 = &[];
        }
        Some(item)
    }
}

fn be32(data: &[u8], at: usize) -> Result<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or(ContainerError::Malformed)
}

fn be64(data: &[u8], at: usize) -> Result<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
        .ok_or(ContainerError::Malformed)
}

fn be16(data: &[u8], at: usize) -> Result<u16> {
    data.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or(ContainerError::Malformed)
}

pub(super) fn open<R: Read + Seek>(reader: &mut R, len: u64) -> Result<Media> {
    let mut pos = 0u64;
    let mut moov = None;
    let mut boxes = 0;
    while pos + 8 <= len {
        boxes += 1;
        if boxes > MAX_TOP_LEVEL_BOXES {
            return Err(ContainerError::Malformed);
        }
        reader.seek(SeekFrom::Start(pos))?;
        let mut head = [0u8; 16];
        let got = read_up_to(reader, &mut head)?;
        let head = &head[..got];
        let kind = [head[4], head[5], head[6], head[7]];
        let (header, size) = match be32(head, 0)? {
            0 => (8, len - pos),
            1 => (16, be64(head, 8)?),
            n => (8, u64::from(n)),
        };
        if size < header || size > len - pos {
            return Err(ContainerError::Malformed);
        }
        match &kind {
            b"moof" => return Err(ContainerError::Fragmented),
            b"moov" if moov.is_none() => {
                if size - header > MAX_METADATA_BYTES {
                    return Err(ContainerError::TooLarge);
                }
                reader.seek(SeekFrom::Start(pos + header))?;
                let mut data = vec![0u8; (size - header) as usize];
                reader.read_exact(&mut data)?;
                moov = Some(data);
            }
            _ => {}
        }
        pos += size;
    }
    parse_moov(&moov.ok_or(ContainerError::Malformed)?, len)
}

enum Entry {
    Avc {
        config: Vec<u8>,
        nal_length_size: usize,
        width: u32,
        height: u32,
    },
    Av1 {
        config: Vec<u8>,
        width: u32,
        height: u32,
    },
    Opus {
        channels: u8,
    },
    Aac,
    OtherAudio,
    OtherVideo,
    Other,
}

#[derive(Default)]
struct Tables<'a> {
    stts: &'a [u8],
    ctts: &'a [u8],
    stss: Option<&'a [u8]>,
    stsz: &'a [u8],
    stsc: &'a [u8],
    stco: &'a [u8],
    co64: &'a [u8],
}

struct Track<'a> {
    entry: Entry,
    timescale: u32,
    /// Start of the presentation inside the media, from the edit list.
    shift: i64,
    tkhd_size: (u32, u32),
    tables: Tables<'a>,
}

fn parse_moov(moov: &[u8], file_len: u64) -> Result<Media> {
    let (mut movie_scale, mut movie_duration) = (0u32, 0u64);
    let (mut video, mut audio): (Option<Track>, Option<Track>) = (None, None);
    let mut audio_skipped = None;
    for child in Boxes(moov) {
        let (kind, body) = child?;
        match &kind {
            b"mvex" => return Err(ContainerError::Fragmented),
            b"mvhd" => {
                let version = *body.first().ok_or(ContainerError::Malformed)?;
                (movie_scale, movie_duration) = if version == 1 {
                    (be32(body, 20)?, be64(body, 24)?)
                } else {
                    (be32(body, 12)?, u64::from(be32(body, 16)?))
                };
            }
            b"trak" => {
                let Some((handler, track)) = parse_trak(body)? else {
                    continue;
                };
                match (&handler, &track.entry) {
                    (b"vide", _) if video.is_none() => video = Some(track),
                    (b"soun", Entry::Aac) => {
                        audio_skipped.get_or_insert(AudioSkipped::Aac);
                    }
                    (b"soun", Entry::Opus { .. }) if audio.is_none() => audio = Some(track),
                    (b"soun", _) => {
                        audio_skipped.get_or_insert(AudioSkipped::Other);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    let mut budget = MAX_SAMPLES;
    let video = video
        .map(|track| -> Result<VideoTrack> {
            let (codec, config, nal_length_size, mut width, mut height) = match track.entry {
                Entry::Avc {
                    ref config,
                    nal_length_size,
                    width,
                    height,
                } => (
                    VideoCodecId::H264,
                    config.clone(),
                    nal_length_size,
                    width,
                    height,
                ),
                Entry::Av1 {
                    ref config,
                    width,
                    height,
                } => (VideoCodecId::Av1, config.clone(), 0, width, height),
                _ => return Err(ContainerError::UnsupportedVideo),
            };
            if width == 0 || height == 0 {
                (width, height) = track.tkhd_size;
            }
            if width == 0 || height == 0 {
                return Err(ContainerError::Malformed);
            }
            let samples = build_samples(&track, true, file_len, &mut budget)?;
            Ok(VideoTrack {
                codec,
                width,
                height,
                samples,
                nal_length_size,
                config,
            })
        })
        .transpose()?;
    let audio = audio
        .map(|track| -> Result<AudioTrack> {
            let Entry::Opus { channels } = track.entry else {
                return Err(ContainerError::NoPlayableTrack);
            };
            Ok(AudioTrack {
                channels,
                samples: build_samples(&track, false, file_len, &mut budget)?,
            })
        })
        .transpose()?;
    let from_samples = [
        video.as_ref().map(|v| &v.samples),
        audio.as_ref().map(|a| &a.samples),
    ]
    .into_iter()
    .flatten()
    .filter_map(|samples| samples.iter().map(|s| u64::from(s.pts_ms)).max())
    .max()
    .unwrap_or(0);
    let from_header = ms_from(i128::from(movie_duration), i128::from(movie_scale));
    Ok(Media {
        duration_ms: u64::from(from_header).max(from_samples),
        video,
        audio,
        audio_skipped,
    })
}

/// The handler type and the sample description of one `trak`; `None` for tracks without
/// the tables a media track has (hint, timecode, ...).
fn parse_trak(trak: &[u8]) -> Result<Option<([u8; 4], Track<'_>)>> {
    let mut tkhd_size = (0, 0);
    let mut shift = 0i64;
    let mut timescale = 0u32;
    let mut handler = None;
    let mut stbl = None;
    for child in Boxes(trak) {
        let (kind, body) = child?;
        match &kind {
            b"tkhd" => {
                let end = body.len();
                if end >= 8 {
                    tkhd_size = (be32(body, end - 8)? >> 16, be32(body, end - 4)? >> 16);
                }
            }
            b"edts" => {
                for edit in Boxes(body) {
                    let (kind, list) = edit?;
                    if &kind != b"elst" {
                        continue;
                    }
                    let version = *list.first().ok_or(ContainerError::Malformed)?;
                    let count = be32(list, 4)?;
                    let (step, time_at) = if version == 1 { (20, 8) } else { (12, 4) };
                    for index in 0..count.min(64) as usize {
                        let at = 8 + index * step + time_at;
                        let time = if version == 1 {
                            be64(list, at)? as i64
                        } else {
                            i64::from(be32(list, at)? as i32)
                        };
                        if time >= 0 {
                            shift = time;
                            break;
                        }
                    }
                }
            }
            b"mdia" => {
                for media in Boxes(body) {
                    let (kind, body) = media?;
                    match &kind {
                        b"mdhd" => {
                            let version = *body.first().ok_or(ContainerError::Malformed)?;
                            timescale = be32(body, if version == 1 { 20 } else { 12 })?;
                        }
                        b"hdlr" => {
                            handler = Some(
                                body.get(8..12)
                                    .ok_or(ContainerError::Malformed)?
                                    .try_into()
                                    .unwrap(),
                            );
                        }
                        b"minf" => {
                            for inner in Boxes(body) {
                                let (kind, body) = inner?;
                                if &kind == b"stbl" {
                                    stbl = Some(body);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    let (Some(handler), Some(stbl)) = (handler, stbl) else {
        return Ok(None);
    };
    if &handler != b"vide" && &handler != b"soun" {
        return Ok(None);
    }
    let mut tables = Tables::default();
    let mut entry = Entry::Other;
    for child in Boxes(stbl) {
        let (kind, body) = child?;
        match &kind {
            b"stsd" => entry = parse_entry(&handler, body)?,
            b"stts" => tables.stts = body,
            b"ctts" => tables.ctts = body,
            b"stss" => tables.stss = Some(body),
            b"stsz" => tables.stsz = body,
            b"stsc" => tables.stsc = body,
            b"stco" => tables.stco = body,
            b"co64" => tables.co64 = body,
            _ => {}
        }
    }
    if timescale == 0 {
        return Err(ContainerError::Malformed);
    }
    Ok(Some((
        handler,
        Track {
            entry,
            timescale,
            shift,
            tkhd_size,
            tables,
        },
    )))
}

fn parse_entry(handler: &[u8; 4], stsd: &[u8]) -> Result<Entry> {
    if be32(stsd, 4)? == 0 {
        return Ok(Entry::Other);
    }
    let Some(first) = Boxes(stsd.get(8..).ok_or(ContainerError::Malformed)?).next() else {
        return Ok(Entry::Other);
    };
    let (kind, body) = first?;
    if handler == b"vide" {
        let (width, height) = (u32::from(be16(body, 24)?), u32::from(be16(body, 26)?));
        let children = body.get(78..).ok_or(ContainerError::Malformed)?;
        let find = |wanted: &[u8; 4]| -> Result<Option<&[u8]>> {
            for child in Boxes(children) {
                let (kind, body) = child?;
                if &kind == wanted {
                    return Ok(Some(body));
                }
            }
            Ok(None)
        };
        return Ok(match &kind {
            b"avc1" | b"avc3" => {
                let record = find(b"avcC")?.ok_or(ContainerError::Malformed)?;
                let (config, nal_length_size) = parse_avcc(record)?;
                Entry::Avc {
                    config,
                    nal_length_size,
                    width,
                    height,
                }
            }
            b"av01" => {
                let record = find(b"av1C")?.ok_or(ContainerError::Malformed)?;
                if record.len() < 4 || record[0] & 0x80 == 0 {
                    return Err(ContainerError::Malformed);
                }
                Entry::Av1 {
                    config: record[4..].to_vec(),
                    width,
                    height,
                }
            }
            _ => Entry::OtherVideo,
        });
    }
    let children = body.get(28..).unwrap_or_default();
    Ok(match &kind {
        b"mp4a" => Entry::Aac,
        b"Opus" => {
            let mut config = None;
            for child in Boxes(children) {
                let (kind, body) = child?;
                if &kind == b"dOps" {
                    config = Some(body);
                }
            }
            match config {
                Some(dops) if dops.len() >= 11 && dops[0] == 0 && dops[10] == 0 => match dops[1] {
                    channels @ 1..=2 => Entry::Opus { channels },
                    _ => Entry::OtherAudio,
                },
                _ => Entry::OtherAudio,
            }
        }
        _ => Entry::OtherAudio,
    })
}

/// Yields one value per sample from `(count, value)` runs.
struct Runs<'a> {
    entries: &'a [u8],
    next: usize,
    left: u32,
    value: i64,
    signed: bool,
}

impl<'a> Runs<'a> {
    fn new(table: &'a [u8], signed: bool) -> Result<Self> {
        let entries = if table.is_empty() {
            &[][..]
        } else {
            let count = be32(table, 4)? as usize;
            table
                .get(8..)
                .and_then(|rest| rest.get(..count.checked_mul(8)?))
                .ok_or(ContainerError::Malformed)?
        };
        Ok(Self {
            entries,
            next: 0,
            left: 0,
            value: 0,
            signed,
        })
    }

    fn take(&mut self) -> Result<i64> {
        while self.left == 0 {
            let Some(entry) = self.entries.get(self.next * 8..self.next * 8 + 8) else {
                return Ok(0);
            };
            self.left = be32(entry, 0)?;
            let raw = be32(entry, 4)?;
            self.value = if self.signed {
                i64::from(raw as i32)
            } else {
                i64::from(raw)
            };
            self.next += 1;
        }
        self.left -= 1;
        Ok(self.value)
    }
}

fn build_samples(
    track: &Track<'_>,
    video: bool,
    file_len: u64,
    budget: &mut usize,
) -> Result<Vec<Sample>> {
    let tables = &track.tables;
    let stsz = tables.stsz;
    if stsz.len() < 12 {
        return Err(ContainerError::Malformed);
    }
    let fixed = be32(stsz, 4)?;
    let count = be32(stsz, 8)? as usize;
    if count > *budget {
        return Err(ContainerError::TooLarge);
    }
    *budget -= count;
    let sizes = if fixed == 0 {
        Some(
            stsz.get(12..)
                .and_then(|rest| rest.get(..count.checked_mul(4)?))
                .ok_or(ContainerError::Malformed)?,
        )
    } else {
        None
    };
    let size_of = |index: usize| -> Result<u32> {
        match sizes {
            Some(sizes) => be32(sizes, index * 4),
            None => Ok(fixed),
        }
    };
    // Chunk offsets.
    let (offsets, wide) = if tables.co64.is_empty() {
        (tables.stco, false)
    } else {
        (tables.co64, true)
    };
    if offsets.len() < 8 {
        return Err(ContainerError::Malformed);
    }
    let chunk_count = be32(offsets, 4)? as usize;
    let width = if wide { 8 } else { 4 };
    let offsets = offsets
        .get(8..)
        .and_then(|rest| rest.get(..chunk_count.checked_mul(width)?))
        .ok_or(ContainerError::Malformed)?;
    let chunk_offset = |index: usize| -> Result<u64> {
        if wide {
            be64(offsets, index * 8)
        } else {
            be32(offsets, index * 4).map(u64::from)
        }
    };
    // Samples per chunk.
    let stsc = tables.stsc;
    if stsc.len() < 8 {
        return Err(ContainerError::Malformed);
    }
    let run_count = be32(stsc, 4)? as usize;
    let runs = stsc
        .get(8..)
        .and_then(|rest| rest.get(..run_count.checked_mul(12)?))
        .ok_or(ContainerError::Malformed)?;
    let run = |index: usize| -> Result<(u32, u32)> {
        Ok((be32(runs, index * 12)?, be32(runs, index * 12 + 4)?))
    };

    let mut stts = Runs::new(tables.stts, false)?;
    let mut ctts = Runs::new(
        tables.ctts,
        tables.ctts.first().is_some_and(|&version| version == 1),
    )?;
    let sync = match tables.stss {
        Some(table) if video => {
            let n = be32(table, 4)? as usize;
            Some(
                table
                    .get(8..)
                    .and_then(|rest| rest.get(..n.checked_mul(4)?))
                    .ok_or(ContainerError::Malformed)?,
            )
        }
        _ => None,
    };
    let mut next_sync = 0usize;
    let timescale = i128::from(track.timescale);
    let mut dts = 0i64;
    let mut samples = Vec::with_capacity(count);
    let mut current_run = 0usize;
    for chunk in 0..chunk_count {
        if samples.len() == count {
            break;
        }
        while current_run + 1 < run_count && run(current_run + 1)?.0 as usize <= chunk + 1 {
            current_run += 1;
        }
        if run_count == 0 || run(current_run)?.0 as usize > chunk + 1 {
            return Err(ContainerError::Malformed);
        }
        let mut offset = chunk_offset(chunk)?;
        for _ in 0..run(current_run)?.1 {
            let index = samples.len();
            if index == count {
                break;
            }
            let size = size_of(index)?;
            let pts = dts
                .checked_add(ctts.take()?)
                .and_then(|t| t.checked_sub(track.shift))
                .ok_or(ContainerError::Malformed)?;
            let number = index as u32 + 1;
            let key = match sync {
                Some(table) if video => {
                    while next_sync * 4 < table.len() && be32(table, next_sync * 4)? < number {
                        next_sync += 1;
                    }
                    next_sync * 4 < table.len() && be32(table, next_sync * 4)? == number
                }
                _ => true,
            };
            samples.push(Sample {
                offset,
                size,
                pts_ms: ms_from(i128::from(pts), timescale),
                key,
            });
            offset = offset
                .checked_add(u64::from(size))
                .ok_or(ContainerError::Malformed)?;
            dts = dts
                .checked_add(stts.take()?)
                .ok_or(ContainerError::Malformed)?;
        }
    }
    if samples.len() != count {
        return Err(ContainerError::Malformed);
    }
    finish_samples(&samples, file_len)?;
    Ok(samples)
}
