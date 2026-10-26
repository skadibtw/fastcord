//! WebM / Matroska indexing. The file is walked element by element with seeks: only
//! `Info` and `Tracks` are read whole; for every block just its few header bytes are read.

use std::io::{Read, Seek, SeekFrom};

use super::{
    AudioCodecId, AudioSkipped, AudioTrack, ContainerError, MAX_METADATA_BYTES, MAX_SAMPLES, Media,
    Sample, VideoCodecId, VideoTrack, aac_lc_info, finish_samples, ms_from, parse_avcc, read_up_to,
};

type Result<T> = std::result::Result<T, ContainerError>;

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const TIMECODE_SCALE: u32 = 0x2A_D7B1;
const DURATION: u32 = 0x4489;
const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const CHANNELS: u32 = 0x9F;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const REFERENCE_BLOCK: u32 = 0xFB;
const DOC_TYPE: u32 = 0x4282;
/// Level-1 elements that end a cluster of unknown size.
const LEVEL1: [u32; 8] = [
    CLUSTER,
    INFO,
    TRACKS,
    0x1C53_BB6B, // Cues
    0x1254_C367, // Tags
    0x1043_A770, // Chapters
    0x1941_A469, // Attachments
    0x114D_9B74, // SeekHead
];

/// Reads a variable-length integer's first byte: `(length, value bits of that byte)`.
fn lead(byte: u8, max: usize) -> Result<(usize, u64)> {
    let length = byte.leading_zeros() as usize + 1;
    if byte == 0 || length > max {
        return Err(ContainerError::Malformed);
    }
    Ok((length, u64::from(byte) & (0xFF >> length)))
}

struct Header {
    id: u32,
    /// `None` for an unknown size.
    size: Option<u64>,
    /// Offset of the first payload byte.
    body: u64,
}

/// Most elements of any kind one file may make the index visit: a file of
/// millions of tiny Void elements must not keep a worker busy (#124).
const MAX_ELEMENTS: usize = 4 * MAX_SAMPLES;
/// The read-ahead window of [`Window`].
const WINDOW_BYTES: usize = 64 * 1024;

/// A read-ahead window over the file that, unlike `BufReader`, keeps its
/// bytes across seeks that land inside them: the index seeks to every
/// element, mostly a few bytes ahead (#124). It also counts the elements.
struct Window<'a, R> {
    inner: &'a mut R,
    buf: Box<[u8]>,
    /// File offset of `buf[0]`, and how much of `buf` holds file bytes.
    start: u64,
    filled: usize,
    pos: u64,
    elements: usize,
}

impl<'a, R: Read + Seek> Window<'a, R> {
    fn new(inner: &'a mut R) -> Self {
        Self {
            inner,
            buf: vec![0; WINDOW_BYTES].into_boxed_slice(),
            start: 0,
            filled: 0,
            pos: 0,
            elements: 0,
        }
    }
}

impl<R: Read + Seek> Read for Window<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        let end = self.start + self.filled as u64;
        if !(self.start..end).contains(&self.pos) {
            self.inner.seek(SeekFrom::Start(self.pos))?;
            if out.len() >= self.buf.len() {
                let n = self.inner.read(out)?;
                self.pos += n as u64;
                return Ok(n);
            }
            self.start = self.pos;
            self.filled = 0;
            self.filled = self.inner.read(&mut self.buf)?;
            if self.filled == 0 {
                return Ok(0);
            }
        }
        let at = (self.pos - self.start) as usize;
        let n = out.len().min(self.filled - at);
        out[..n].copy_from_slice(&self.buf[at..at + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for Window<'_, R> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = match to {
            SeekFrom::Start(at) => at,
            SeekFrom::Current(by) => self
                .pos
                .checked_add_signed(by)
                .ok_or(std::io::ErrorKind::InvalidInput)?,
            SeekFrom::End(_) => self.inner.seek(to)?,
        };
        Ok(self.pos)
    }
}

fn header<R: Read + Seek>(r: &mut Window<'_, R>, at: u64, len: u64) -> Result<Option<Header>> {
    if at >= len {
        return Ok(None);
    }
    r.elements += 1;
    if r.elements > MAX_ELEMENTS {
        return Err(ContainerError::TooLarge);
    }
    r.seek(SeekFrom::Start(at))?;
    let mut buf = [0u8; 12];
    let got = read_up_to(r, &mut buf)?;
    let buf = &buf[..got];
    let first = *buf.first().ok_or(ContainerError::Malformed)?;
    let id_len = lead(first, 4)?.0;
    let id = buf
        .get(..id_len)
        .ok_or(ContainerError::Malformed)?
        .iter()
        .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
    let (size, size_len) = vint(buf.get(id_len..).unwrap_or_default())?;
    Ok(Some(Header {
        id,
        size,
        body: at + (id_len + size_len) as u64,
    }))
}

/// A size vint: `(value or None for unknown, encoded length)`.
fn vint(data: &[u8]) -> Result<(Option<u64>, usize)> {
    let first = *data.first().ok_or(ContainerError::Malformed)?;
    let (length, mut value) = lead(first, 8)?;
    let bytes = data.get(1..length).ok_or(ContainerError::Malformed)?;
    let mut all_ones = value == 0xFF >> length;
    for &b in bytes {
        value = (value << 8) | u64::from(b);
        all_ones &= b == 0xFF;
    }
    Ok(((!all_ones).then_some(value), length))
}

/// The child elements `(id, payload)` of an in-memory element payload.
struct Elements<'a>(&'a [u8]);

impl<'a> Iterator for Elements<'a> {
    type Item = Result<(u32, &'a [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let item = (|| {
            let id_len = lead(self.0[0], 4)?.0;
            let id = self
                .0
                .get(..id_len)
                .ok_or(ContainerError::Malformed)?
                .iter()
                .fold(0u32, |acc, &b| (acc << 8) | u32::from(b));
            let (size, size_len) = vint(&self.0[id_len..])?;
            let start = id_len + size_len;
            let size = usize::try_from(size.ok_or(ContainerError::Malformed)?)
                .map_err(|_| ContainerError::Malformed)?;
            let payload = self
                .0
                .get(start..start.checked_add(size).ok_or(ContainerError::Malformed)?)
                .ok_or(ContainerError::Malformed)?;
            self.0 = &self.0[start + size..];
            Ok((id, payload))
        })();
        if item.is_err() {
            self.0 = &[];
        }
        Some(item)
    }
}

fn uint(data: &[u8]) -> Result<u64> {
    if data.len() > 8 {
        return Err(ContainerError::Malformed);
    }
    Ok(data.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
}

fn float(data: &[u8]) -> Result<f64> {
    match data.len() {
        4 => Ok(f64::from(f32::from_be_bytes(data.try_into().unwrap()))),
        8 => Ok(f64::from_be_bytes(data.try_into().unwrap())),
        _ => Err(ContainerError::Malformed),
    }
}

fn read_payload<R: Read + Seek>(r: &mut R, head: &Header, limit: u64) -> Result<Vec<u8>> {
    let size = head.size.ok_or(ContainerError::Malformed)?;
    if size > MAX_METADATA_BYTES {
        return Err(ContainerError::TooLarge);
    }
    if head.body.checked_add(size).is_none_or(|end| end > limit) {
        return Err(ContainerError::Malformed);
    }
    r.seek(SeekFrom::Start(head.body))?;
    let mut data = vec![0u8; size as usize];
    r.read_exact(&mut data)?;
    Ok(data)
}

enum Selected {
    Video {
        number: u64,
        codec: VideoCodecId,
        config: Vec<u8>,
        nal_length_size: usize,
        width: u32,
        height: u32,
    },
    Audio {
        number: u64,
        codec: AudioCodecId,
        channels: u8,
        rate: u32,
        config: Vec<u8>,
    },
}

#[derive(Default)]
struct Tracks {
    video: Option<Selected>,
    audio: Option<Selected>,
    audio_skipped: Option<AudioSkipped>,
}

fn parse_tracks(payload: &[u8]) -> Result<Tracks> {
    let mut tracks = Tracks::default();
    for entry in Elements(payload) {
        let (id, body) = entry?;
        if id != TRACK_ENTRY {
            continue;
        }
        let (mut number, mut kind, mut codec, mut private) = (0, 0, &[][..], &[][..]);
        let (mut width, mut height, mut channels) = (0u32, 0u32, 1u64);
        let mut encoded = false;
        for field in Elements(body) {
            let (id, data) = field?;
            match id {
                TRACK_NUMBER => number = uint(data)?,
                TRACK_TYPE => kind = uint(data)?,
                CODEC_ID => codec = data,
                CODEC_PRIVATE => private = data,
                CONTENT_ENCODINGS => encoded = true,
                VIDEO => {
                    for dim in Elements(data) {
                        let (id, data) = dim?;
                        match id {
                            PIXEL_WIDTH => width = u32::try_from(uint(data)?).unwrap_or(0),
                            PIXEL_HEIGHT => height = u32::try_from(uint(data)?).unwrap_or(0),
                            _ => {}
                        }
                    }
                }
                AUDIO => {
                    for field in Elements(data) {
                        let (id, data) = field?;
                        if id == CHANNELS {
                            channels = uint(data)?;
                        }
                    }
                }
                _ => {}
            }
        }
        match kind {
            1 if tracks.video.is_none() => {
                if encoded || width == 0 || height == 0 {
                    return Err(ContainerError::UnsupportedVideo);
                }
                tracks.video = Some(match codec {
                    b"V_MPEG4/ISO/AVC" => {
                        let (config, nal_length_size) = parse_avcc(private)?;
                        Selected::Video {
                            number,
                            codec: VideoCodecId::H264,
                            config,
                            nal_length_size,
                            width,
                            height,
                        }
                    }
                    b"V_AV1" => Selected::Video {
                        number,
                        codec: VideoCodecId::Av1,
                        config: private.get(4..).unwrap_or_default().to_vec(),
                        nal_length_size: 0,
                        width,
                        height,
                    },
                    _ => return Err(ContainerError::UnsupportedVideo),
                });
            }
            2 if tracks.audio.is_none() => {
                let opus = codec == b"A_OPUS"
                    && !encoded
                    && matches!(channels, 1 | 2)
                    && (private.len() < 19 || private[18] == 0);
                let aac = (codec == b"A_AAC" && !encoded)
                    .then(|| aac_lc_info(private))
                    .flatten();
                if opus {
                    tracks.audio = Some(Selected::Audio {
                        number,
                        codec: AudioCodecId::Opus,
                        channels: channels as u8,
                        rate: 48_000,
                        config: Vec::new(),
                    });
                } else if let Some((rate, channels)) = aac {
                    tracks.audio = Some(Selected::Audio {
                        number,
                        codec: AudioCodecId::Aac,
                        channels,
                        rate,
                        config: private.to_vec(),
                    });
                } else {
                    tracks.audio_skipped.get_or_insert(if codec == b"A_AAC" {
                        AudioSkipped::Aac
                    } else {
                        AudioSkipped::Other
                    });
                }
            }
            _ => {}
        }
    }
    Ok(tracks)
}

struct Scan {
    video_number: Option<u64>,
    audio_number: Option<u64>,
    scale_ns: i128,
    video: Vec<Sample>,
    audio: Vec<Sample>,
    audio_laced: bool,
    budget: usize,
}

impl Scan {
    fn add(&mut self, track: u64, sample: Sample) -> Result<()> {
        let target = if Some(track) == self.video_number {
            &mut self.video
        } else if Some(track) == self.audio_number {
            &mut self.audio
        } else {
            return Ok(());
        };
        if self.budget == 0 {
            return Err(ContainerError::TooLarge);
        }
        self.budget -= 1;
        target.push(sample);
        Ok(())
    }
}

/// A block's track, sample (its `key` is the SimpleBlock flag) and whether it is laced.
fn parse_block<R: Read + Seek>(
    r: &mut Window<'_, R>,
    head: &Header,
    cluster: i64,
    scale_ns: i128,
) -> Result<Option<(u64, Sample, bool)>> {
    let size = head.size.ok_or(ContainerError::Malformed)?;
    let mut buf = [0u8; 12];
    r.seek(SeekFrom::Start(head.body))?;
    let got = read_up_to(r, &mut buf[..(size.min(12) as usize)])?;
    let buf = &buf[..got];
    let (track, track_len) = {
        let (value, length) = vint(buf)?;
        (value.ok_or(ContainerError::Malformed)?, length)
    };
    let fixed = track_len + 3;
    if size < fixed as u64 || buf.len() < fixed {
        return Err(ContainerError::Malformed);
    }
    let timecode = i64::from(i16::from_be_bytes([buf[track_len], buf[track_len + 1]]));
    let flags = buf[track_len + 2];
    let ticks = i128::from(
        cluster
            .checked_add(timecode)
            .ok_or(ContainerError::Malformed)?,
    ) * scale_ns;
    Ok(Some((
        track,
        Sample {
            offset: head.body + fixed as u64,
            size: u32::try_from(size - fixed as u64).map_err(|_| ContainerError::TooLarge)?,
            pts_ms: ms_from(ticks, 1_000_000_000),
            key: flags & 0x80 != 0,
        },
        flags & 0x06 != 0,
    )))
}

/// Walks one cluster whose payload starts at `start`, returns the offset after it.
fn scan_cluster<R: Read + Seek>(
    r: &mut Window<'_, R>,
    start: u64,
    end: Option<u64>,
    len: u64,
    scan: &mut Scan,
) -> Result<u64> {
    let limit = end.unwrap_or(len);
    let (mut pos, mut cluster) = (start, 0i64);
    while pos < limit {
        let Some(head) = header(r, pos, len)? else {
            break;
        };
        if end.is_none() && LEVEL1.contains(&head.id) {
            return Ok(pos);
        }
        let size = head.size.ok_or(ContainerError::Malformed)?;
        let next = head
            .body
            .checked_add(size)
            .filter(|&next| next <= limit)
            .ok_or(ContainerError::Malformed)?;
        match head.id {
            TIMECODE => {
                let data = read_payload(r, &head, limit)?;
                cluster = i64::try_from(uint(&data)?).map_err(|_| ContainerError::Malformed)?;
            }
            SIMPLE_BLOCK => {
                if let Some((track, sample, laced)) = parse_block(r, &head, cluster, scan.scale_ns)?
                {
                    add_block(scan, track, sample, laced, None)?;
                }
            }
            BLOCK_GROUP => {
                let (mut block, mut referenced) = (None, false);
                let mut at = head.body;
                while at < next {
                    let Some(child) = header(r, at, len)? else {
                        break;
                    };
                    let child_size = child.size.ok_or(ContainerError::Malformed)?;
                    let after = child
                        .body
                        .checked_add(child_size)
                        .filter(|&after| after <= next)
                        .ok_or(ContainerError::Malformed)?;
                    match child.id {
                        BLOCK => block = parse_block(r, &child, cluster, scan.scale_ns)?,
                        REFERENCE_BLOCK => referenced = true,
                        _ => {}
                    }
                    at = after;
                }
                if let Some((track, sample, laced)) = block {
                    add_block(scan, track, sample, laced, Some(!referenced))?;
                }
            }
            _ => {}
        }
        pos = next;
    }
    Ok(limit.min(pos.max(start)))
}

fn add_block(
    scan: &mut Scan,
    track: u64,
    mut sample: Sample,
    laced: bool,
    group_key: Option<bool>,
) -> Result<()> {
    if Some(track) == scan.video_number {
        if laced {
            return Err(ContainerError::UnsupportedVideo);
        }
        if let Some(key) = group_key {
            sample.key = key;
        }
    } else if Some(track) == scan.audio_number {
        scan.audio_laced |= laced;
        sample.key = true;
    }
    scan.add(track, sample)
}

pub(super) fn open<R: Read + Seek>(r: &mut R, len: u64) -> Result<Media> {
    let r = &mut Window::new(r);
    let ebml = header(r, 0, len)?.ok_or(ContainerError::Malformed)?;
    let ebml_size = ebml.size.ok_or(ContainerError::Malformed)?;
    if ebml.id != EBML || ebml_size > 1024 {
        return Err(ContainerError::UnknownFormat);
    }
    for field in Elements(&read_payload(r, &ebml, len)?) {
        let (id, data) = field?;
        if id == DOC_TYPE && data != b"webm" && data != b"matroska" {
            return Err(ContainerError::UnknownFormat);
        }
    }
    let mut pos = ebml.body + ebml_size;
    let segment = header(r, pos, len)?.ok_or(ContainerError::Malformed)?;
    if segment.id != SEGMENT {
        return Err(ContainerError::Malformed);
    }
    pos = segment.body;
    let end = segment
        .size
        .map_or(len, |size| segment.body.saturating_add(size).min(len));
    let (mut scale, mut info_duration) = (1_000_000u64, None);
    let mut tracks = None;
    let mut scan = Scan {
        video_number: None,
        audio_number: None,
        scale_ns: 1_000_000,
        video: Vec::new(),
        audio: Vec::new(),
        audio_laced: false,
        budget: MAX_SAMPLES,
    };
    while pos < end {
        let Some(head) = header(r, pos, len)? else {
            break;
        };
        let after = match head.id {
            INFO => {
                let data = read_payload(r, &head, end)?;
                for field in Elements(&data) {
                    let (id, data) = field?;
                    match id {
                        TIMECODE_SCALE => scale = uint(data)?.max(1),
                        DURATION => info_duration = Some(float(data)?),
                        _ => {}
                    }
                }
                scan.scale_ns = i128::from(scale);
                head.body + head.size.unwrap_or_default()
            }
            TRACKS => {
                let data = read_payload(r, &head, end)?;
                let parsed = parse_tracks(&data)?;
                scan.video_number = match parsed.video {
                    Some(Selected::Video { number, .. }) => Some(number),
                    _ => None,
                };
                scan.audio_number = match parsed.audio {
                    Some(Selected::Audio { number, .. }) => Some(number),
                    _ => None,
                };
                tracks = Some(parsed);
                head.body + head.size.unwrap_or_default()
            }
            CLUSTER => {
                if tracks.is_none() {
                    return Err(ContainerError::Malformed);
                }
                let bound = head
                    .size
                    .map(|size| head.body.saturating_add(size))
                    .map(|bound| {
                        if bound > end {
                            Err(ContainerError::Malformed)
                        } else {
                            Ok(bound)
                        }
                    })
                    .transpose()?;
                scan_cluster(r, head.body, bound, end, &mut scan)?
            }
            _ => {
                let size = head.size.ok_or(ContainerError::Malformed)?;
                head.body
                    .checked_add(size)
                    .ok_or(ContainerError::Malformed)?
            }
        };
        if after <= pos {
            return Err(ContainerError::Malformed);
        }
        pos = after;
    }
    let tracks = tracks.ok_or(ContainerError::Malformed)?;
    finish_samples(&scan.video, len)?;
    finish_samples(&scan.audio, len)?;
    let mut audio_skipped = tracks.audio_skipped;
    let video = match tracks.video {
        Some(Selected::Video {
            codec,
            config,
            nal_length_size,
            width,
            height,
            ..
        }) => {
            if scan.video.is_empty() {
                return Err(ContainerError::Malformed);
            }
            Some(VideoTrack {
                codec,
                width,
                height,
                samples: std::mem::take(&mut scan.video),
                nal_length_size,
                config,
            })
        }
        _ => None,
    };
    let audio = match tracks.audio {
        Some(Selected::Audio {
            codec,
            channels,
            rate,
            config,
            ..
        }) if !scan.audio_laced && !scan.audio.is_empty() => Some(AudioTrack {
            codec,
            channels,
            rate,
            config,
            samples: std::mem::take(&mut scan.audio),
        }),
        Some(_) => {
            audio_skipped = Some(AudioSkipped::Other);
            None
        }
        None => None,
    };
    let from_samples = [
        video.as_ref().map(|v| &v.samples),
        audio.as_ref().map(|a| &a.samples),
    ]
    .into_iter()
    .flatten()
    .filter_map(|samples| samples.iter().map(|s| u64::from(s.pts_ms)).max())
    .max()
    .unwrap_or(0);
    let from_info = info_duration
        .filter(|d| d.is_finite() && *d > 0.0)
        .map_or(0, |d| {
            (d * scale as f64 / 1_000_000.0).min(f64::from(u32::MAX)) as u64
        });
    Ok(Media {
        duration_ms: from_info.max(from_samples),
        video,
        audio,
        audio_skipped,
    })
}
