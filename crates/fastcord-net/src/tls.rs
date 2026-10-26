//! Best-effort, application-local shaping of the plaintext TLS handshake.
//!
//! This never changes handshake bytes. Record boundaries and asynchronous
//! writes are different things: a write split does not promise TCP segments.
//! `flush` is used only to deliver buffered bytes, never to force a packet.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};
use tokio::time::Sleep;

pub const CATALOG_VERSION: u32 = 4;
pub const MAX_CLIENT_HELLO: usize = 32 * 1024;
/// Records the first plaintext ClientHello is refragmented into by
/// `Strategy::RecordFragment`.
const FRAGMENTS: usize = 4;
/// Up to thirty one-byte SNI suffix records, plus prefix and suffix.
const MAX_SNI_FRAGMENT_BYTES: usize = 30;
const MAX_FRAGMENT_STEPS: usize = MAX_SNI_FRAGMENT_BYTES + 2;
const SNI_WRITE_DELAY: Duration = Duration::from_millis(30);
/// Raw staging (<64 KiB) plus temporary handshake assembly (32 KiB).
pub const MAX_STAGING_BYTES: usize = 96 * 1024;
// Leave slack for the tiny shared HRR flag and pending writer metadata.
const MAX_RAW: usize = MAX_STAGING_BYTES - MAX_CLIENT_HELLO - 256;
const MAX_RECORD: usize = 16 * 1024;
const WRITE_DELAY: Duration = Duration::from_millis(10);
const HRR_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91,
    0xc2, 0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Strategy {
    #[default]
    Direct,
    TlsRecordSplit,
    WriteSplit,
    Combined,
    MultiSplit,
    PacketFake,
    PacketOverlap,
    /// Legal TLS refragmentation of the plaintext ClientHello whose write
    /// boundaries deliberately avoid every record boundary. A filter that
    /// reassembles records from a segment starting on a record header never
    /// sees a parseable ClientHello, which is what defeats SNI blackholing on
    /// routes where TCP splitting and two-record splitting alone do not.
    RecordFragment,
    /// Fragment the final thirty SNI bytes into one-byte records with 30 ms
    /// between off-boundary writes, preserving all handshake bytes.
    SniFragment,
}

impl Strategy {
    pub const ORDER: [Self; 6] = [
        Self::Direct,
        Self::RecordFragment,
        Self::SniFragment,
        Self::TlsRecordSplit,
        Self::WriteSplit,
        Self::Combined,
    ];
    #[cfg(test)]
    pub const LEGACY: [Self; 6] = [
        Self::Direct,
        Self::TlsRecordSplit,
        Self::WriteSplit,
        Self::Combined,
        Self::RecordFragment,
        Self::SniFragment,
    ];

    const fn record_split(self) -> bool {
        matches!(self, Self::TlsRecordSplit | Self::Combined)
    }

    const fn write_split(self) -> bool {
        matches!(self, Self::WriteSplit | Self::Combined)
    }
}

/// Categorical errors contain neither TLS bytes nor names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    InvalidRecord,
    InvalidHandshake,
    HelloTooLarge,
    MissingSni,
    InvalidSni,
    StagingLimit,
}

/// Offsets in the original record stream, not in a guessed TCP segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientHello {
    pub split_at: usize,
    pub sni_mid: usize,
    pub sni_end: usize,
    pub record_start: usize,
    pub record_end: usize,
    pub prefix_end: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Inspection {
    Incomplete,
    ClientHello(ClientHello),
    OtherHandshake,
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<usize> {
    let field = bytes.get(offset..offset.checked_add(2)?)?;
    Some(usize::from(u16::from_be_bytes([field[0], field[1]])))
}

/// Parses a complete structural ClientHello across arbitrarily split records.
/// The caller may append more bytes after `Incomplete`. This allocates at most
/// 32 KiB temporarily; record overhead is separately bounded to 64 KiB.
pub fn inspect_client_hello(records: &[u8]) -> Result<Inspection, ParseError> {
    if records.len() > MAX_RAW {
        return Err(ParseError::StagingLimit);
    }
    let mut hello = Vec::new();
    let mut offset = 0;
    let mut expected = None;
    loop {
        let Some(header) = records.get(offset..offset + 5) else {
            return Ok(Inspection::Incomplete);
        };
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if header[0] != 22
            || header[1] != 3
            || !(1..=3).contains(&header[2])
            || length == 0
            || length > MAX_RECORD
        {
            return Err(ParseError::InvalidRecord);
        }
        let end = offset + 5 + length;
        let Some(payload) = records.get(offset + 5..end) else {
            return Ok(Inspection::Incomplete);
        };
        if hello.is_empty() && payload[0] != 1 {
            return Ok(Inspection::OtherHandshake);
        }
        // Reserve once to keep capacity bounded even with one-byte records.
        if hello.capacity() == 0 {
            hello.reserve_exact(MAX_CLIENT_HELLO);
        }
        for &byte in payload {
            if hello.len() == MAX_CLIENT_HELLO {
                return Err(ParseError::HelloTooLarge);
            }
            hello.push(byte);
            if hello.len() == 4 {
                let length = (usize::from(hello[1]) << 16)
                    | (usize::from(hello[2]) << 8)
                    | usize::from(hello[3]);
                let total = length + 4;
                if total > MAX_CLIENT_HELLO {
                    return Err(ParseError::HelloTooLarge);
                }
                expected = Some(total);
            }
            if expected == Some(hello.len()) {
                let (logical_split, middle, sni_end) = sni_split(&hello)?;
                let (record_start, split_at, record_end) =
                    locate(records, logical_split).ok_or(ParseError::InvalidRecord)?;
                return Ok(Inspection::ClientHello(ClientHello {
                    split_at,
                    sni_mid: locate(records, middle).ok_or(ParseError::InvalidRecord)?.1,
                    sni_end: locate(records, sni_end).ok_or(ParseError::InvalidRecord)?.1,
                    record_start,
                    record_end,
                    prefix_end: end,
                }));
            }
        }
        offset = end;
    }
}

fn locate(records: &[u8], mut logical_offset: usize) -> Option<(usize, usize, usize)> {
    let mut offset = 0;
    loop {
        let length = u16_at(records, offset + 3)?;
        let end = offset.checked_add(5 + length)?;
        records.get(offset..end)?;
        if logical_offset <= length {
            return Some((offset, offset + 5 + logical_offset, end));
        }
        logical_offset -= length;
        offset = end;
    }
}

fn sni_split(hello: &[u8]) -> Result<(usize, usize, usize), ParseError> {
    let invalid = ParseError::InvalidHandshake;
    // Handshake header, legacy_version and random.
    let mut offset = 4 + 2 + 32;
    let session = usize::from(*hello.get(offset).ok_or(invalid)?);
    if session > 32 {
        return Err(invalid);
    }
    offset += 1 + session;
    let ciphers = u16_at(hello, offset).ok_or(invalid)?;
    if ciphers < 2 || ciphers % 2 != 0 {
        return Err(invalid);
    }
    offset += 2 + ciphers;
    let compression = usize::from(*hello.get(offset).ok_or(invalid)?);
    if compression == 0 {
        return Err(invalid);
    }
    offset += 1 + compression;
    let extensions = u16_at(hello, offset).ok_or(invalid)?;
    offset += 2;
    if offset + extensions != hello.len() {
        return Err(invalid);
    }
    let mut split = None;
    while offset < hello.len() {
        let kind = u16_at(hello, offset).ok_or(invalid)?;
        let length = u16_at(hello, offset + 2).ok_or(invalid)?;
        offset += 4;
        let end = offset.checked_add(length).ok_or(invalid)?;
        let extension = hello.get(offset..end).ok_or(invalid)?;
        if kind == 0 {
            if split.is_some() {
                return Err(ParseError::InvalidSni);
            }
            let names = u16_at(extension, 0).ok_or(ParseError::InvalidSni)?;
            if names + 2 != extension.len() {
                return Err(ParseError::InvalidSni);
            }
            let mut name = 2;
            let mut host = None;
            while name < extension.len() {
                let name_type = extension[name];
                let length = u16_at(extension, name + 1).ok_or(ParseError::InvalidSni)?;
                name += 3;
                let bytes = extension
                    .get(name..name + length)
                    .ok_or(ParseError::InvalidSni)?;
                if name_type == 0 {
                    if bytes.is_empty() || bytes.len() > 253 || host.is_some() {
                        return Err(ParseError::InvalidSni);
                    }
                    // Middle of the second-last DNS label (midsld), without
                    // changing or retaining the hostname outside this parse.
                    let end = bytes
                        .iter()
                        .rposition(|b| *b == b'.')
                        .unwrap_or(bytes.len());
                    let start = bytes[..end]
                        .iter()
                        .rposition(|b| *b == b'.')
                        .map_or(0, |i| i + 1);
                    let middle = (start + (end - start).div_ceil(2)).clamp(1, bytes.len());
                    host = Some((
                        offset + name + 1,
                        offset + name + middle,
                        offset + name + length,
                    ));
                }
                name += length;
            }
            split = Some(host.ok_or(ParseError::InvalidSni)?);
        }
        offset = end;
    }
    split.ok_or(ParseError::MissingSni)
}

/// Rewrites only a TLS record header and inserts a continuation record header.
/// The handshake payload, including ALPN and PSK binders, is unchanged.
pub fn split_record(bytes: &mut Vec<u8>, hello: ClientHello) -> Result<(), ParseError> {
    let header_end = hello
        .record_start
        .checked_add(5)
        .ok_or(ParseError::InvalidRecord)?;
    let header = bytes
        .get(hello.record_start..header_end)
        .ok_or(ParseError::InvalidRecord)?;
    let version = [header[1], header[2]];
    let original = usize::from(u16::from_be_bytes([header[3], header[4]]));
    if header[0] != 22
        || Some(hello.record_end) != header_end.checked_add(original)
        || hello.record_end > bytes.len()
        || hello.split_at <= header_end
        || hello.split_at > hello.record_end
    {
        return Err(ParseError::InvalidRecord);
    }
    // Already at a record boundary: do not introduce an empty record.
    if hello.split_at == hello.record_end {
        return Ok(());
    }
    if bytes.len() + 5 > MAX_RAW {
        return Err(ParseError::StagingLimit);
    }
    let prefix = hello.split_at - header_end;
    let suffix = hello.record_end - hello.split_at;
    let prefix = u16::try_from(prefix).map_err(|_| ParseError::InvalidRecord)?;
    let suffix = u16::try_from(suffix).map_err(|_| ParseError::InvalidRecord)?;
    bytes[hello.record_start + 3..hello.record_start + 5].copy_from_slice(&prefix.to_be_bytes());
    let continuation = [
        22,
        version[0],
        version[1],
        suffix.to_be_bytes()[0],
        suffix.to_be_bytes()[1],
    ];
    if bytes.capacity() < bytes.len() + 5 {
        bytes.reserve_exact(5);
    }
    bytes.splice(hello.split_at..hello.split_at, continuation);
    Ok(())
}

/// Rewrites one plaintext record into `FRAGMENTS` records that carry exactly
/// the same handshake bytes. The returned offsets are the new record boundaries,
/// so the caller can keep its write boundaries one byte short of each of them.
/// Handshake, SNI, ALPN and the transcript are untouched: only record headers
/// and their lengths differ, and a peer reassembles the message as usual. The
/// rewrite inserts `5 * (FRAGMENTS - 1)` bytes in place, so staging keeps its
/// one bounded buffer instead of copying the hello twice.
pub fn fragment_record(bytes: &mut Vec<u8>, hello: ClientHello) -> Result<Vec<usize>, ParseError> {
    let header_end = hello
        .record_start
        .checked_add(5)
        .ok_or(ParseError::InvalidRecord)?;
    let payload = hello
        .record_end
        .checked_sub(header_end)
        .ok_or(ParseError::InvalidRecord)?;
    if payload < 2 * FRAGMENTS || hello.record_end > bytes.len() {
        return Err(ParseError::InvalidRecord);
    }
    let cuts: Vec<_> = (1..FRAGMENTS)
        .map(|index| header_end + payload * index / FRAGMENTS)
        .collect();
    fragment_record_at(bytes, hello, &cuts)
}

/// Only the structurally parsed hostname is split. The bounded suffix covers
/// Discord's service domains even when a voice server adds a long prefix.
fn fragment_sni_record(bytes: &mut Vec<u8>, hello: ClientHello) -> Result<Vec<usize>, ParseError> {
    let start = hello
        .split_at
        .saturating_sub(1)
        .max(hello.sni_end.saturating_sub(MAX_SNI_FRAGMENT_BYTES));
    if hello.sni_end > hello.record_end || start >= hello.sni_end {
        return Err(ParseError::InvalidSni);
    }
    let cuts: Vec<_> = (start..=hello.sni_end)
        .filter(|cut| *cut > hello.record_start + 5 && *cut < hello.record_end)
        .collect();
    fragment_record_at(bytes, hello, &cuts)
}

fn fragment_record_at(
    bytes: &mut Vec<u8>,
    hello: ClientHello,
    cuts: &[usize],
) -> Result<Vec<usize>, ParseError> {
    let header_end = hello
        .record_start
        .checked_add(5)
        .ok_or(ParseError::InvalidRecord)?;
    let header: [u8; 5] = bytes
        .get(hello.record_start..header_end)
        .ok_or(ParseError::InvalidRecord)?
        .try_into()
        .map_err(|_| ParseError::InvalidRecord)?;
    let original = usize::from(u16::from_be_bytes([header[3], header[4]]));
    if header[0] != 22
        || Some(hello.record_end) != header_end.checked_add(original)
        || hello.record_end > bytes.len()
        || cuts.is_empty()
        || cuts.len() >= MAX_FRAGMENT_STEPS
        || cuts[0] <= header_end
        || cuts[cuts.len() - 1] >= hello.record_end
        || cuts.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(ParseError::InvalidRecord);
    }
    if bytes.len() + 5 * cuts.len() > MAX_RAW {
        return Err(ParseError::StagingLimit);
    }
    // Validate every cut before mutating; transformed output can never roll back.
    for position in cuts.iter().rev() {
        bytes.splice(*position..*position, [22, header[1], header[2], 0, 0]);
    }
    let mut boundaries = Vec::with_capacity(cuts.len() + 1);
    let mut start = header_end;
    for (index, cut) in cuts.iter().copied().chain([hello.record_end]).enumerate() {
        let head = if index == 0 {
            hello.record_start
        } else {
            cuts[index - 1] + 5 * (index - 1)
        };
        let length = u16::try_from(cut - start).map_err(|_| ParseError::InvalidRecord)?;
        bytes[head + 3..head + 5].copy_from_slice(&length.to_be_bytes());
        start = cut;
        boundaries.push(head + 5 + usize::from(length));
    }
    Ok(boundaries)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Scan,
    Pass,
    Failed,
}

/// Fixed-size observation of the first incoming ServerHello prefix. Its
/// reserved HRR random is the only reason to permit plaintext ClientHello
/// shaping after a compatibility CCS. In TLS 1.2 abbreviated handshakes, CCS
/// instead starts encryption of type-22 Finished records, which must never be
/// parsed or transformed even if ciphertext resembles a ClientHello.
#[derive(Clone)]
pub struct ServerHelloObserver {
    hrr: Option<Arc<AtomicBool>>,
    header: [u8; 5],
    header_len: usize,
    remaining: usize,
    kind: u8,
    handshake: [u8; 38],
    handshake_len: usize,
    finished: bool,
}

impl ServerHelloObserver {
    fn new(enabled: bool) -> Self {
        Self {
            hrr: enabled.then(|| Arc::new(AtomicBool::new(false))),
            header: [0; 5],
            header_len: 0,
            remaining: 0,
            kind: 0,
            handshake: [0; 38],
            handshake_len: 0,
            finished: !enabled,
        }
    }

    pub fn observe(&mut self, incoming: &[u8]) {
        if self.finished {
            return;
        }
        for &byte in incoming {
            if self.remaining == 0 {
                self.header[self.header_len] = byte;
                self.header_len += 1;
                if self.header_len == 5 {
                    self.header_len = 0;
                    self.remaining =
                        usize::from(u16::from_be_bytes([self.header[3], self.header[4]]));
                    self.kind = self.header[0];
                    if self.header[1] != 3
                        || self.remaining == 0
                        || self.remaining > MAX_RECORD + 256
                        || !matches!(self.kind, 20 | 22)
                    {
                        self.finished = true;
                        return;
                    }
                }
                continue;
            }
            self.remaining -= 1;
            if self.kind == 22 {
                self.handshake[self.handshake_len] = byte;
                self.handshake_len += 1;
                if self.handshake[0] != 2 {
                    self.finished = true;
                    return;
                }
                if self.handshake_len == self.handshake.len() {
                    if self.handshake[6..] == HRR_RANDOM
                        && let Some(hrr) = self.hrr.as_ref()
                    {
                        hrr.store(true, Ordering::Release);
                    }
                    self.finished = true;
                    return;
                }
            }
        }
    }

    fn retry_hello_allowed(&self) -> bool {
        self.hrr
            .as_ref()
            .is_some_and(|hrr| hrr.load(Ordering::Acquire))
    }
}

/// A capability for one already-connected, application-owned socket. Public
/// sends account for their physical bytes in the same probe budget as writes.
pub trait PrefixControl: Send + Sync {
    fn close(&self);
    fn start_fake(
        &self,
        _original: Vec<u8>,
        _fake: Vec<u8>,
        _strategy: Strategy,
    ) -> io::Result<PrefixFuture> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

pub type PrefixFuture = Pin<Box<dyn Future<Output = io::Result<usize>> + Send>>;
pub trait FakePrefixFactory: Send + Sync {
    /// False once the privilege boundary behind this factory is gone (a helper
    /// that exited, a pipe that closed). Selection then stops choosing its
    /// strategy instead of failing every dial with it for the rest of the
    /// process; the ordinary catalog takes over.
    fn available(&self) -> bool {
        true
    }
    fn strategy(&self) -> Strategy {
        Strategy::PacketFake
    }
    fn select_strategy(&self, _strategy: Strategy) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn start(
        &self,
        socket: std::net::TcpStream,
        original: Vec<u8>,
        fake: Vec<u8>,
        strategy: Strategy,
    ) -> io::Result<PrefixFuture>;
}

#[derive(Clone, Copy, Default)]
enum SendMode {
    #[default]
    Ordinary,
    Fake,
}
#[derive(Clone, Copy, Default)]
struct Step {
    end: usize,
    mode: SendMode,
    pause: bool,
}
impl Step {
    fn new(end: usize, mode: SendMode, pause: bool) -> Self {
        Self { end, mode, pause }
    }
}

pub struct ShapedStream<S> {
    inner: S,
    strategy: Strategy,
    mode: Mode,
    bytes: Vec<u8>,
    end: usize,
    written: usize,
    steps: [Step; MAX_FRAGMENT_STEPS],
    step_count: usize,
    step_index: usize,
    control: Option<Arc<dyn PrefixControl>>,
    fake: Option<PrefixFuture>,
    fake_data: Vec<u8>,
    delay: Option<Pin<Box<Sleep>>>,
    hellos: u8,
    transformed: bool,
    budget: Option<Arc<Semaphore>>,
    acquiring: Option<PermitFuture>,
    permit: Option<OwnedSemaphorePermit>,
    server_hello: ServerHelloObserver,
}

type PermitFuture =
    Pin<Box<dyn Future<Output = Result<OwnedSemaphorePermit, AcquireError>> + Send>>;

impl<S> ShapedStream<S> {
    pub fn new(inner: S, strategy: Strategy) -> Self {
        Self {
            inner,
            strategy,
            mode: if strategy == Strategy::Direct {
                Mode::Pass
            } else {
                Mode::Scan
            },
            bytes: Vec::new(),
            end: 0,
            written: 0,
            steps: [Step::default(); MAX_FRAGMENT_STEPS],
            step_count: 0,
            step_index: 0,
            control: None,
            fake: None,
            fake_data: Vec::new(),
            delay: None,
            hellos: 0,
            transformed: false,
            budget: None,
            acquiring: None,
            permit: None,
            server_hello: ServerHelloObserver::new(strategy != Strategy::Direct),
        }
    }

    /// Application-wide staging concurrency, acquired before allocating and
    /// released as soon as each handshake prefix has reached the socket.
    pub fn with_budget(inner: S, strategy: Strategy, budget: Arc<Semaphore>) -> Self {
        let mut stream = Self::new(inner, strategy);
        stream.budget = Some(budget);
        stream
    }

    pub fn with_control(mut self, control: Arc<dyn PrefixControl>) -> Self {
        self.control = Some(control);
        self
    }

    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// An opaque bridge feeds its independent upstream reader through this
    /// observer before forwarding those bytes to the TLS client. Ordinary TLS
    /// streams observe their own AsyncRead automatically.
    pub fn server_hello_observer(&self) -> ServerHelloObserver {
        self.server_hello.clone()
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    pub fn into_inner(self) -> S {
        self.inner
    }

    fn invalid(&mut self) -> io::Error {
        self.mode = Mode::Failed;
        self.bytes = Vec::new();
        self.end = 0;
        self.written = 0;
        self.step_count = 0;
        self.step_index = 0;
        self.fake = None;
        self.fake_data = Vec::new();
        self.delay = None;
        self.acquiring = None;
        self.permit = None;
        io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported TLS handshake prefix",
        )
    }

    fn unsupported(&mut self) -> io::Result<()> {
        if self.transformed {
            return Err(self.invalid());
        }
        self.mode = Mode::Pass;
        self.end = self.bytes.len();
        Ok(())
    }

    fn prepare(&mut self) -> io::Result<()> {
        if self.mode == Mode::Failed {
            return Err(self.invalid());
        }
        if self.end != 0 || self.bytes.is_empty() {
            return Ok(());
        }
        if self.mode == Mode::Pass {
            self.end = self.bytes.len();
            return Ok(());
        }
        if self.bytes.len() < 5 {
            return Ok(());
        }
        match self.bytes[0] {
            23 | 21 => {
                // Protected TLS 1.3 data or an alert ends prefix shaping.
                self.mode = Mode::Pass;
                self.end = self.bytes.len();
            }
            20 => {
                // TLS 1.3's compatibility CCS may precede the HRR response.
                if !self.server_hello.retry_hello_allowed() {
                    self.mode = Mode::Pass;
                    self.end = self.bytes.len();
                    return Ok(());
                }
                if self.bytes[1..5] != [3, 3, 0, 1] {
                    return self.unsupported();
                }
                if self.bytes.len() >= 6 {
                    if self.bytes[5] != 1 {
                        return self.unsupported();
                    }
                    self.end = 6;
                }
            }
            22 => match inspect_client_hello(&self.bytes) {
                Ok(Inspection::Incomplete) => {
                    if self.bytes.len() == MAX_RAW {
                        return self.unsupported();
                    }
                }
                Ok(Inspection::OtherHandshake) => {
                    // TLS 1.2 client key exchange and subsequent protected
                    // handshake records must pass without being interpreted.
                    self.mode = Mode::Pass;
                    self.end = self.bytes.len();
                }
                Ok(Inspection::ClientHello(hello)) => {
                    let old_len = self.bytes.len();
                    if self.strategy.record_split() && split_record(&mut self.bytes, hello).is_err()
                    {
                        return self.unsupported();
                    }
                    let fragments = if matches!(
                        self.strategy,
                        Strategy::RecordFragment | Strategy::SniFragment
                    ) {
                        let result = if self.strategy == Strategy::SniFragment {
                            fragment_sni_record(&mut self.bytes, hello)
                        } else {
                            fragment_record(&mut self.bytes, hello)
                        };
                        match result {
                            Ok(boundaries) => Some(boundaries),
                            Err(_) => return self.unsupported(),
                        }
                    } else {
                        None
                    };
                    self.end = hello.prefix_end + self.bytes.len() - old_len;
                    match fragments {
                        Some(boundaries) => self.plan_fragments(&boundaries),
                        None => self.plan(hello)?,
                    }
                    self.transformed = true;
                    self.hellos += 1;
                    if self.hellos == 2 {
                        self.mode = Mode::Pass;
                    }
                }
                Err(_) => return self.unsupported(),
            },
            _ => return self.unsupported(),
        }
        Ok(())
    }

    /// One write per fragment, each ending one byte before the next record
    /// boundary. A segment that does not begin on a record header cannot be
    /// parsed as a record stream by a filtering middlebox, and the bytes still
    /// reassemble into the untouched handshake. The final write carries the
    /// rest of the staged prefix, so no write ends on a record boundary.
    fn plan_fragments(&mut self, boundaries: &[usize]) {
        let interior = boundaries.len().saturating_sub(1).min(self.steps.len() - 1);
        for (index, boundary) in boundaries[..interior].iter().enumerate() {
            self.steps[index] = Step::new(boundary.saturating_sub(1), SendMode::Ordinary, true);
        }
        self.steps[interior] = Step::new(self.end, SendMode::Ordinary, false);
        self.step_count = interior + 1;
    }

    fn plan(&mut self, hello: ClientHello) -> io::Result<()> {
        use SendMode::{Fake, Ordinary};
        if matches!(
            self.strategy,
            Strategy::PacketFake | Strategy::PacketOverlap
        ) && self.control.is_none()
        {
            return Err(io::ErrorKind::Unsupported.into());
        }
        let first = hello.split_at;
        if self.strategy.write_split() {
            self.steps[..2].copy_from_slice(&[
                Step::new(first, Ordinary, true),
                Step::new(self.end, Ordinary, false),
            ]);
            self.step_count = 2;
        }
        match self.strategy {
            Strategy::PacketFake | Strategy::PacketOverlap => {
                if self.end > 1024 || u16_at(&self.bytes, 3).map(|n| n + 5) != Some(self.end) {
                    return Err(io::ErrorKind::Unsupported.into());
                }
                self.fake_data = self.bytes[..self.end].to_vec();
                let start = first - 1;
                let length = u16_at(&self.bytes, start - 2).ok_or(io::ErrorKind::InvalidData)?;
                if length == 0 || start + length > self.end {
                    return Err(io::ErrorKind::Unsupported.into());
                }
                self.fake_data[start..start + length].fill(b'a');
                if length >= 11 {
                    self.fake_data[start + length - 11..start + length]
                        .copy_from_slice(b"example.com");
                }
                if length > 11 {
                    self.fake_data[start + length - 12] = b'.';
                }
                self.steps[0] = Step::new(self.end, Fake, false);
                self.step_count = 1;
            }
            Strategy::MultiSplit => {
                self.steps[..3].copy_from_slice(&[
                    Step::new(1, Ordinary, true),
                    Step::new(hello.sni_mid, Ordinary, true),
                    Step::new(self.end, Ordinary, false),
                ]);
                self.step_count = 3;
            }
            _ => {}
        }
        Ok(())
    }
}

impl<S: AsyncWrite + Unpin> ShapedStream<S> {
    /// For an opaque CONNECT pump whose reader is independent of this writer:
    /// forward a complete prefix, but keep an incomplete ClientHello staged
    /// while the pump awaits the next client read. Unlike AsyncWrite::flush,
    /// this makes no promise that an incomplete prefix has become visible.
    /// The server-to-client direction must use the unwrapped read half.
    pub async fn drain_ready(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| {
            std::task::ready!(self.poll_drain(cx))?;
            Pin::new(&mut self.inner).poll_flush(cx)
        })
        .await
    }

    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            self.prepare()?;
            if self.end == 0 {
                return Poll::Ready(Ok(()));
            }
            while self.written < self.end {
                let step = if self.step_index < self.step_count {
                    self.steps[self.step_index]
                } else {
                    Step::new(self.end, SendMode::Ordinary, false)
                };
                let wrote = if self.written < step.end {
                    let bytes = &self.bytes[self.written..step.end];
                    if matches!(step.mode, SendMode::Fake) {
                        if self.fake.is_none() {
                            let control =
                                self.control.as_ref().ok_or(io::ErrorKind::Unsupported)?;
                            self.fake = Some(control.start_fake(
                                bytes.to_vec(),
                                std::mem::take(&mut self.fake_data),
                                self.strategy,
                            )?);
                        }
                        let result = std::task::ready!(
                            self.fake
                                .as_mut()
                                .ok_or(io::ErrorKind::InvalidData)?
                                .as_mut()
                                .poll(cx)
                        )?;
                        self.fake = None;
                        if result != bytes.len() {
                            if let Some(control) = &self.control {
                                control.close();
                            }
                            return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
                        }
                        result
                    } else {
                        std::task::ready!(Pin::new(&mut self.inner).poll_write(cx, bytes))?
                    }
                } else {
                    0
                };
                if self.written == step.end {
                    if step.pause {
                        let delay = self.delay.get_or_insert_with(|| {
                            Box::pin(tokio::time::sleep(
                                if self.strategy == Strategy::SniFragment {
                                    SNI_WRITE_DELAY
                                } else {
                                    WRITE_DELAY
                                },
                            ))
                        });
                        std::task::ready!(delay.as_mut().poll(cx));
                        self.delay = None;
                    }
                    self.step_index += 1;
                    continue;
                }
                if wrote == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                self.written += wrote;
            }
            self.bytes.drain(..self.end);
            self.end = 0;
            self.written = 0;
            self.step_count = 0;
            self.step_index = 0;
            if self.bytes.is_empty() {
                // A healthy idle connection retains no shaping allocation.
                self.bytes = Vec::new();
                self.permit = None;
                return Poll::Ready(Ok(()));
            }
        }
    }

    fn poll_visible(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        std::task::ready!(self.poll_drain(cx))?;
        if !self.bytes.is_empty() {
            self.unsupported()?;
            std::task::ready!(self.poll_drain(cx))?;
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for ShapedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_visible(cx))?;
        let before = buffer.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(()))) {
            this.server_hello.observe(&buffer.filled()[before..]);
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ShapedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_drain(cx))?;
        if this.mode == Mode::Pass {
            return Pin::new(&mut this.inner).poll_write(cx, input);
        }
        if this.mode == Mode::Failed {
            return Poll::Ready(Err(this.invalid()));
        }
        if input.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if this.permit.is_none()
            && let Some(budget) = this.budget.as_ref()
        {
            let acquiring = this
                .acquiring
                .get_or_insert_with(|| Box::pin(budget.clone().acquire_owned()));
            let permit = std::task::ready!(acquiring.as_mut().poll(cx))
                .map_err(|_| io::Error::other("TLS shaping budget closed"))?;
            this.permit = Some(permit);
            this.acquiring = None;
        }
        if this.bytes.capacity() == 0 {
            this.bytes.reserve_exact(MAX_RAW);
        }
        let count = input.len().min(MAX_RAW - this.bytes.len());
        if count == 0 {
            return Poll::Ready(Err(this.invalid()));
        }
        this.bytes.extend_from_slice(&input[..count]);
        this.prepare()?;
        // Once accepted, report precisely the caller's bytes, excluding any
        // inserted record header. A Pending result never hides accepted input.
        Poll::Ready(Ok(count))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_visible(cx))?;
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_visible(cx))?;
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "tls/tests.rs"]
mod tests;
