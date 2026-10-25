//! RFC 7692 `permessage-deflate` as a byte adapter under `tungstenite`.
//!
//! `tungstenite` 0.30 has no extension support and rejects any frame with an
//! RSV bit set, so it cannot be taught about deflate through its API. This
//! adapter sits between the TLS stream and `WebSocketStream`: the handshake is
//! done by [`crate::handshake`], the adapter parses the raw frames only as far
//! as needed to inflate RSV1 messages and deflate outgoing ones, and
//! `tungstenite` keeps owning everything else (masking rules, control frames,
//! UTF-8, message and frame limits, close handshake).
//!
//! When the server declines the extension, the adapter is a pure pass-through:
//! no parsing, buffering or allocation happens on that connection.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use rand_core::{OsRng, RngCore};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::Error as WsError;

use crate::extension::PerMessageDeflate;

/// Buffers larger than this are released instead of kept between messages
/// (READY-class events must not pin their memory).
const RETAINED_SCRATCH_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 32 * 1024;
const READ_CHUNK_BYTES: usize = 16 * 1024;
const MAX_HEADER_EXTRA: usize = 14;
const SYNC_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];
const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xa;

/// Byte ceilings for one connection. The frame limit is enforced before a
/// payload is buffered; the message limit bounds the inflated output and the
/// compressed accumulation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_message_bytes: usize,
    pub max_frame_bytes: usize,
}

impl Limits {
    pub const fn new(max_message_bytes: usize, max_frame_bytes: usize) -> Self {
        Self {
            max_message_bytes,
            max_frame_bytes,
        }
    }

    /// A compressed message can be a few bytes larger than its input; the
    /// accumulation cap allows for deflate's stored-block overhead only.
    const fn max_compressed_bytes(self) -> usize {
        self.max_message_bytes
            .saturating_add(self.max_message_bytes / 8)
            .saturating_add(128)
    }
}

/// A malformed or oversized permessage-deflate stream. It is a typed failure,
/// never a panic; crate callers can recognise it through
/// [`adapter_error`]/[`classify`] even after `tungstenite` wrapped it in
/// [`WsError::Io`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterError {
    /// A frame declared a payload larger than the configured frame limit.
    FrameTooLarge,
    /// The inflated message exceeded the caller's message limit.
    MessageTooLarge,
    /// Compressed input for one message exceeded the accumulation cap.
    CompressedTooLarge,
    /// RSV1 on a continuation or control frame, or RSV2/RSV3 anywhere.
    InvalidReservedBits,
    /// A continuation frame with no message in progress.
    UnexpectedContinuation,
    /// A new data frame arrived while a fragmented message was incomplete.
    ExpectedContinuation,
    /// The server sent a masked frame (RFC 6455 requires it not to).
    MaskedServerFrame,
    /// An opcode RFC 6455 does not define.
    InvalidOpcode,
    /// A control frame was fragmented, too long, or carried RSV1.
    InvalidControlFrame,
    /// The deflate stream is not valid for this connection.
    CorruptDeflate,
    /// The OS could not provide a mask key.
    Entropy,
    /// The frame header promised bytes the stream never delivered.
    UnexpectedEof,
    /// The inner stream refused a zero-length write.
    WriteZero,
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::FrameTooLarge => "WebSocket frame exceeds the configured limit",
            Self::MessageTooLarge => "inflated WebSocket message exceeds the limit",
            Self::CompressedTooLarge => "compressed WebSocket message exceeds the limit",
            Self::InvalidReservedBits => "reserved WebSocket bits are set",
            Self::UnexpectedContinuation => "continuation frame without a message",
            Self::ExpectedContinuation => "new data frame inside a fragmented message",
            Self::MaskedServerFrame => "masked frame from the server",
            Self::InvalidOpcode => "unknown WebSocket opcode",
            Self::InvalidControlFrame => "invalid WebSocket control frame",
            Self::CorruptDeflate => "invalid permessage-deflate stream",
            Self::Entropy => "could not obtain a WebSocket mask",
            Self::UnexpectedEof => "WebSocket frame truncated by the peer",
            Self::WriteZero => "WebSocket stream accepted no bytes",
        })
    }
}

impl std::error::Error for AdapterError {}

impl AdapterError {
    fn into_io(self) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, self)
    }
}

/// Recovers the adapter's typed cause from an I/O error.
pub fn adapter_error(error: &io::Error) -> Option<AdapterError> {
    error.get_ref()?.downcast_ref::<AdapterError>().copied()
}

/// Recovers the adapter's typed cause after `tungstenite` wrapped it.
pub fn classify(error: &WsError) -> Option<AdapterError> {
    match error {
        WsError::Io(error) => adapter_error(error),
        _ => None,
    }
}

/// True for adapter failures that mean "this message is too large", which the
/// Gateway maps to its own event-size stop reason.
pub fn is_size_limit(error: &WsError) -> bool {
    matches!(
        classify(error),
        Some(AdapterError::FrameTooLarge)
            | Some(AdapterError::MessageTooLarge)
            | Some(AdapterError::CompressedTooLarge)
    )
}

/// A `WebSocketStream`-compatible byte stream carrying an optional
/// permessage-deflate connection.
pub struct CompressingStream<S> {
    inner: S,
    limits: Limits,
    mode: Mode,
    /// Raw frames received from the inner stream and not yet processed.
    in_buf: Vec<u8>,
    in_pos: usize,
    /// Transformed frames ready to be handed to `tungstenite`.
    out: Vec<u8>,
    out_pos: usize,
    chunk: Box<[u8]>,
}

enum Mode {
    PassThrough {
        /// Bytes over-read while parsing the handshake response.
        preload: Vec<u8>,
        pos: usize,
    },
    Deflated(Box<Deflated>),
}

struct Deflated {
    pmd: PerMessageDeflate,
    max_compressed: usize,
    deflate: Compress,
    inflate: Decompress,
    /// Inbound compressed bytes of the message being assembled.
    compressed: Vec<u8>,
    /// Inbound inflated message.
    inflated: Vec<u8>,
    /// Outbound compressed scratch.
    outgoing: Vec<u8>,
    /// The fragmented data message in progress, if any.
    in_progress: Option<DataMessage>,
    /// Raw client frames written by `tungstenite` and not yet transformed.
    w_in: Vec<u8>,
    w_in_pos: usize,
    /// Transformed client frames ready for the inner stream.
    w_out: Vec<u8>,
    w_out_pos: usize,
}

#[derive(Clone, Copy, Debug)]
struct DataMessage {
    opcode: u8,
    compressed: bool,
}

impl<S> CompressingStream<S> {
    /// `None` means the server declined the extension: the stream is a pure
    /// pass-through.
    pub fn new(inner: S, pmd: Option<PerMessageDeflate>, limits: Limits) -> Self {
        let mode = match pmd {
            None => Mode::PassThrough {
                preload: Vec::new(),
                pos: 0,
            },
            Some(pmd) => {
                let deflated = Deflated {
                    pmd,
                    max_compressed: limits.max_compressed_bytes(),
                    deflate: Compress::new(Compression::default(), false),
                    inflate: Decompress::new(false),
                    compressed: Vec::new(),
                    inflated: Vec::new(),
                    outgoing: Vec::new(),
                    in_progress: None,
                    w_in: Vec::new(),
                    w_in_pos: 0,
                    w_out: Vec::new(),
                    w_out_pos: 0,
                };
                Mode::Deflated(Box::new(deflated))
            }
        };
        Self {
            inner,
            limits,
            mode,
            in_buf: Vec::new(),
            in_pos: 0,
            out: Vec::new(),
            out_pos: 0,
            chunk: vec![0; CHUNK_BYTES].into_boxed_slice(),
        }
    }

    /// Feeds bytes already read past the handshake response.
    pub fn with_preloaded(mut self, bytes: Vec<u8>) -> Self {
        match &mut self.mode {
            Mode::PassThrough { preload, pos } => {
                *preload = bytes;
                *pos = 0;
            }
            Mode::Deflated(_) => self.in_buf = bytes,
        }
        self
    }

    /// The negotiated parameters, for tests and diagnostics.
    pub fn negotiated(&self) -> Option<PerMessageDeflate> {
        match &self.mode {
            Mode::PassThrough { .. } => None,
            Mode::Deflated(deflated) => Some(deflated.pmd),
        }
    }

    fn trim_out(&mut self) {
        if self.out.capacity() > RETAINED_SCRATCH_BYTES {
            self.out = Vec::new();
        }
    }

    fn compact_in(&mut self) {
        if self.in_pos == 0 {
            return;
        }
        self.in_buf.drain(..self.in_pos);
        self.in_pos = 0;
        if self.in_buf.capacity() > RETAINED_SCRATCH_BYTES && self.in_buf.is_empty() {
            self.in_buf = Vec::new();
        }
    }

    /// Consumes one complete inbound frame. Returns `false` when more bytes are
    /// needed.
    fn process_inbound(&mut self) -> Result<bool, AdapterError> {
        let Self {
            mode,
            in_buf,
            in_pos,
            out,
            chunk,
            limits,
            ..
        } = self;
        let Mode::Deflated(deflated) = mode else {
            return Ok(false);
        };
        let available = &in_buf[*in_pos..];
        let Some(header) = parse_header(available, limits.max_frame_bytes, false)? else {
            return Ok(false);
        };
        let total = header.header_len + header.payload_len;
        if available.len() < total {
            return Ok(false);
        }
        let payload_start = *in_pos + header.header_len;
        let payload_end = *in_pos + total;
        if header.opcode >= 0x8 && (header.rsv1 || !header.fin || header.payload_len > 125) {
            return Err(AdapterError::InvalidControlFrame);
        }
        match header.opcode {
            OPCODE_CLOSE | OPCODE_PING | OPCODE_PONG => {
                out.extend_from_slice(&in_buf[*in_pos..payload_end]);
            }
            OPCODE_CONTINUATION => {
                let Some(message) = deflated.in_progress else {
                    return Err(AdapterError::UnexpectedContinuation);
                };
                if header.rsv1 {
                    return Err(AdapterError::InvalidReservedBits);
                }
                if message.compressed {
                    let len = deflated
                        .compressed
                        .len()
                        .checked_add(header.payload_len)
                        .ok_or(AdapterError::CompressedTooLarge)?;
                    if len > deflated.max_compressed {
                        return Err(AdapterError::CompressedTooLarge);
                    }
                    deflated
                        .compressed
                        .extend_from_slice(&in_buf[payload_start..payload_end]);
                } else {
                    out.extend_from_slice(&in_buf[*in_pos..payload_end]);
                }
                if header.fin {
                    if message.compressed {
                        finish_inflated(deflated, out, &mut chunk[..], limits.max_message_bytes)?;
                    }
                    deflated.in_progress = None;
                }
            }
            OPCODE_TEXT | OPCODE_BINARY => {
                if deflated.in_progress.is_some() {
                    return Err(AdapterError::ExpectedContinuation);
                }
                if header.rsv1 {
                    if header.payload_len > deflated.max_compressed {
                        return Err(AdapterError::CompressedTooLarge);
                    }
                    deflated.compressed.clear();
                    deflated
                        .compressed
                        .extend_from_slice(&in_buf[payload_start..payload_end]);
                    deflated.in_progress = Some(DataMessage {
                        opcode: header.opcode,
                        compressed: true,
                    });
                    if header.fin {
                        finish_inflated(deflated, out, &mut chunk[..], limits.max_message_bytes)?;
                        deflated.in_progress = None;
                    }
                } else {
                    out.extend_from_slice(&in_buf[*in_pos..payload_end]);
                    if !header.fin {
                        deflated.in_progress = Some(DataMessage {
                            opcode: header.opcode,
                            compressed: false,
                        });
                    }
                }
            }
            _ => return Err(AdapterError::InvalidOpcode),
        }
        *in_pos += total;
        Ok(true)
    }

    /// Consumes complete outbound client frames written by `tungstenite`.
    fn process_outbound(&mut self) -> Result<(), AdapterError> {
        let Self {
            mode,
            chunk,
            limits,
            ..
        } = self;
        let Mode::Deflated(deflated) = mode else {
            return Ok(());
        };
        loop {
            let available = &deflated.w_in[deflated.w_in_pos..];
            let Some(header) = parse_header(available, limits.max_frame_bytes, true)? else {
                break;
            };
            let total = header.header_len + header.payload_len;
            if available.len() < total {
                break;
            }
            let payload_start = deflated.w_in_pos + header.header_len;
            let payload_end = deflated.w_in_pos + total;
            let passthrough = header.opcode >= 0x8
                || header.opcode == OPCODE_CONTINUATION
                || !header.fin
                || header.rsv1
                || !deflated.pmd.client_may_compress();
            if passthrough {
                let frame = &deflated.w_in[deflated.w_in_pos..payload_end];
                deflated.w_out.extend_from_slice(frame);
            } else {
                if let Some(mask) = header.mask {
                    for (index, byte) in deflated.w_in[payload_start..payload_end]
                        .iter_mut()
                        .enumerate()
                    {
                        *byte ^= mask[index & 3];
                    }
                }
                deflate_message(
                    &mut deflated.deflate,
                    &deflated.w_in[payload_start..payload_end],
                    &mut deflated.outgoing,
                    &mut chunk[..],
                    limits.max_message_bytes,
                )?;
                let mask = mask_key()?;
                append_client_frame(&mut deflated.w_out, header.opcode, &deflated.outgoing, mask);
                if deflated.pmd.client_no_context_takeover {
                    deflated.deflate.reset();
                }
                if deflated.outgoing.capacity() > RETAINED_SCRATCH_BYTES {
                    deflated.outgoing = Vec::new();
                }
            }
            deflated.w_in_pos += total;
        }
        if deflated.w_in_pos > 0 {
            deflated.w_in.drain(..deflated.w_in_pos);
            deflated.w_in_pos = 0;
            if deflated.w_in.capacity() > RETAINED_SCRATCH_BYTES && deflated.w_in.is_empty() {
                deflated.w_in = Vec::new();
            }
        }
        if deflated.w_in.len() > limits.max_frame_bytes.saturating_add(MAX_HEADER_EXTRA) {
            return Err(AdapterError::FrameTooLarge);
        }
        Ok(())
    }
}

impl<S: AsyncWrite + Unpin> CompressingStream<S> {
    /// Writes transformed client frames to the inner stream.
    fn poll_write_outbound(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let Self { mode, inner, .. } = self;
        let Mode::Deflated(deflated) = mode else {
            return Poll::Ready(Ok(()));
        };
        while deflated.w_out_pos < deflated.w_out.len() {
            match Pin::new(&mut *inner).poll_write(cx, &deflated.w_out[deflated.w_out_pos..]) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(AdapterError::WriteZero.into_io()));
                }
                Poll::Ready(Ok(written)) => deflated.w_out_pos += written,
            }
        }
        deflated.w_out.clear();
        deflated.w_out_pos = 0;
        if deflated.w_out.capacity() > RETAINED_SCRATCH_BYTES {
            deflated.w_out = Vec::new();
        }
        Poll::Ready(Ok(()))
    }

    /// Completes any transformed writes; used before shutdown and by flush.
    fn finish_outbound(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Err(error) = self.process_outbound() {
            return Poll::Ready(Err(error.into_io()));
        }
        self.poll_write_outbound(cx)
    }
}

/// Inflates the assembled message into a single frame with RSV1 cleared.
fn finish_inflated(
    deflated: &mut Deflated,
    out: &mut Vec<u8>,
    chunk: &mut [u8],
    max_message_bytes: usize,
) -> Result<(), AdapterError> {
    let opcode = deflated
        .in_progress
        .filter(|message| message.compressed)
        .map(|message| message.opcode)
        .ok_or(AdapterError::CorruptDeflate)?;
    deflated.compressed.extend_from_slice(&SYNC_SUFFIX);
    inflate_raw(
        &mut deflated.inflate,
        &deflated.compressed,
        &mut deflated.inflated,
        chunk,
        max_message_bytes,
    )?;
    if deflated.pmd.server_no_context_takeover {
        deflated.inflate.reset(false);
    }
    deflated.compressed.clear();
    if deflated.compressed.capacity() > RETAINED_SCRATCH_BYTES {
        deflated.compressed = Vec::new();
    }
    append_server_frame(out, opcode, &deflated.inflated);
    deflated.inflated.clear();
    if deflated.inflated.capacity() > RETAINED_SCRATCH_BYTES {
        deflated.inflated = Vec::new();
    }
    Ok(())
}

/// Inflates one RFC 7692 message (`input` must already end with the sync-flush
/// suffix) into `out`, enforcing `max_output`. Shared with the test peer.
pub(crate) fn inflate_raw(
    inflate: &mut Decompress,
    input: &[u8],
    out: &mut Vec<u8>,
    chunk: &mut [u8],
    max_output: usize,
) -> Result<(), AdapterError> {
    out.clear();
    if out.capacity() > RETAINED_SCRATCH_BYTES {
        *out = Vec::new();
    }
    let mut input: &[u8] = input;
    loop {
        let before_in = inflate.total_in();
        let before_out = inflate.total_out();
        let status = inflate
            .decompress(input, chunk, FlushDecompress::Sync)
            .map_err(|_| AdapterError::CorruptDeflate)?;
        let consumed = (inflate.total_in() - before_in) as usize;
        let produced = (inflate.total_out() - before_out) as usize;
        input = &input[consumed..];
        if produced > max_output.saturating_sub(out.len()) {
            return Err(AdapterError::MessageTooLarge);
        }
        out.extend_from_slice(&chunk[..produced]);
        match status {
            // A live permessage-deflate connection never ends the deflate
            // stream, even with `*_no_context_takeover`.
            Status::StreamEnd => return Err(AdapterError::CorruptDeflate),
            Status::Ok | Status::BufError => {
                if input.is_empty() && produced < chunk.len() {
                    break;
                }
                if consumed == 0 && produced == 0 {
                    return Err(AdapterError::CorruptDeflate);
                }
            }
        }
    }
    Ok(())
}

/// Raw deflate with `Z_SYNC_FLUSH`, minus the RFC 7692 suffix.
pub(crate) fn deflate_message(
    deflate: &mut Compress,
    input: &[u8],
    out: &mut Vec<u8>,
    chunk: &mut [u8],
    max_output: usize,
) -> Result<(), AdapterError> {
    out.clear();
    let limit = max_output.saturating_add(max_output / 8).saturating_add(64);
    let mut remaining = input;
    loop {
        let before_in = deflate.total_in();
        let before_out = deflate.total_out();
        let status = deflate
            .compress(remaining, chunk, FlushCompress::Sync)
            .map_err(|_| AdapterError::CorruptDeflate)?;
        let consumed = (deflate.total_in() - before_in) as usize;
        let produced = (deflate.total_out() - before_out) as usize;
        remaining = &remaining[consumed..];
        if produced > limit.saturating_sub(out.len()) {
            return Err(AdapterError::CompressedTooLarge);
        }
        out.extend_from_slice(&chunk[..produced]);
        match status {
            Status::StreamEnd => return Err(AdapterError::CorruptDeflate),
            Status::Ok | Status::BufError => {
                if remaining.is_empty() && produced == 0 {
                    // The sync flush completed without new output.
                    break;
                }
                if remaining.is_empty() && status == Status::Ok && produced < chunk.len() {
                    break;
                }
                if consumed == 0 && produced == 0 {
                    return Err(AdapterError::CorruptDeflate);
                }
            }
        }
    }
    if !out.ends_with(&SYNC_SUFFIX) {
        return Err(AdapterError::CorruptDeflate);
    }
    out.truncate(out.len() - SYNC_SUFFIX.len());
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedHeader {
    fin: bool,
    rsv1: bool,
    opcode: u8,
    payload_len: usize,
    header_len: usize,
    mask: Option<[u8; 4]>,
}

fn parse_header(
    bytes: &[u8],
    max_frame_bytes: usize,
    allow_mask: bool,
) -> Result<Option<ParsedHeader>, AdapterError> {
    if bytes.len() < 2 {
        return Ok(None);
    }
    let first = bytes[0];
    let second = bytes[1];
    if first & 0x30 != 0 {
        return Err(AdapterError::InvalidReservedBits);
    }
    let fin = first & 0x80 != 0;
    let rsv1 = first & 0x40 != 0;
    let opcode = first & 0x0f;
    if !matches!(
        opcode,
        OPCODE_CONTINUATION
            | OPCODE_TEXT
            | OPCODE_BINARY
            | OPCODE_CLOSE
            | OPCODE_PING
            | OPCODE_PONG
    ) {
        return Err(AdapterError::InvalidOpcode);
    }
    let masked = second & 0x80 != 0;
    if masked && !allow_mask {
        return Err(AdapterError::MaskedServerFrame);
    }
    let length = (second & 0x7f) as usize;
    let (payload_len, mut header_len) = match length {
        126 => {
            if bytes.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([bytes[2], bytes[3]]) as usize, 4)
        }
        127 => {
            if bytes.len() < 10 {
                return Ok(None);
            }
            let mut length_bytes = [0u8; 8];
            length_bytes.copy_from_slice(&bytes[2..10]);
            if length_bytes[0] & 0x80 != 0 {
                return Err(AdapterError::FrameTooLarge);
            }
            let length = u64::from_be_bytes(length_bytes);
            if length > usize::MAX as u64 {
                return Err(AdapterError::FrameTooLarge);
            }
            (length as usize, 10)
        }
        length => (length, 2),
    };
    if opcode >= 0x8 && (!fin || rsv1 || payload_len > 125) {
        return Err(AdapterError::InvalidControlFrame);
    }
    if payload_len > max_frame_bytes {
        return Err(AdapterError::FrameTooLarge);
    }
    let mask = if masked {
        if bytes.len() < header_len + 4 {
            return Ok(None);
        }
        let mut key = [0u8; 4];
        key.copy_from_slice(&bytes[header_len..header_len + 4]);
        header_len += 4;
        Some(key)
    } else {
        None
    };
    Ok(Some(ParsedHeader {
        fin,
        rsv1,
        opcode,
        payload_len,
        header_len,
        mask,
    }))
}

fn append_server_frame(out: &mut Vec<u8>, opcode: u8, payload: &[u8]) {
    out.push(0x80 | opcode);
    append_length(out, payload.len(), false);
    out.extend_from_slice(payload);
}

fn append_client_frame(out: &mut Vec<u8>, opcode: u8, payload: &[u8], mask: [u8; 4]) {
    // Every call compresses, so FIN and RSV1 are both set.
    out.push(0xC0 | opcode);
    append_length(out, payload.len(), true);
    out.extend_from_slice(&mask);
    for (index, byte) in payload.iter().enumerate() {
        out.push(byte ^ mask[index & 3]);
    }
}

fn append_length(out: &mut Vec<u8>, length: usize, masked: bool) {
    let mask = if masked { 0x80 } else { 0x00 };
    if length < 126 {
        out.push(mask | length as u8);
    } else if length <= u16::MAX as usize {
        out.push(mask | 126);
        out.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        out.push(mask | 127);
        out.extend_from_slice(&(length as u64).to_be_bytes());
    }
}

fn mask_key() -> Result<[u8; 4], AdapterError> {
    let mut key = [0u8; 4];
    let mut rng = OsRng;
    rng.try_fill_bytes(&mut key)
        .map_err(|_| AdapterError::Entropy)?;
    Ok(key)
}

impl<S: AsyncRead + Unpin> AsyncRead for CompressingStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.out_pos < this.out.len() {
                let count = (this.out.len() - this.out_pos).min(buf.remaining());
                buf.put_slice(&this.out[this.out_pos..this.out_pos + count]);
                this.out_pos += count;
                if this.out_pos == this.out.len() {
                    this.out.clear();
                    this.out_pos = 0;
                    this.trim_out();
                }
                return Poll::Ready(Ok(()));
            }
            match &mut this.mode {
                Mode::PassThrough { preload, pos } => {
                    if *pos < preload.len() {
                        let count = (preload.len() - *pos).min(buf.remaining());
                        buf.put_slice(&preload[*pos..*pos + count]);
                        *pos += count;
                        if *pos == preload.len() {
                            preload.clear();
                            *pos = 0;
                        }
                        return Poll::Ready(Ok(()));
                    }
                    return Pin::new(&mut this.inner).poll_read(cx, buf);
                }
                Mode::Deflated(_) => {}
            }
            match this.process_inbound() {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => return Poll::Ready(Err(error.into_io())),
            }
            let mut temp = [0u8; READ_CHUNK_BYTES];
            let mut read = ReadBuf::new(&mut temp);
            match Pin::new(&mut this.inner).poll_read(cx, &mut read) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {
                    let filled = read.filled().len();
                    if filled == 0 {
                        if this.in_pos < this.in_buf.len() {
                            return Poll::Ready(Err(AdapterError::UnexpectedEof.into_io()));
                        }
                        return Poll::Ready(Ok(()));
                    }
                    this.in_buf.extend_from_slice(read.filled());
                    this.compact_in();
                }
            }
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CompressingStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match &mut this.mode {
            Mode::PassThrough { .. } => Pin::new(&mut this.inner).poll_write(cx, buf),
            Mode::Deflated(_) => {
                if let Err(error) = this.process_outbound() {
                    return Poll::Ready(Err(error.into_io()));
                }
                // Best effort; `flush` (always called after a tungstenite
                // write) completes the transformed queue.
                if let Poll::Ready(Err(error)) = this.poll_write_outbound(cx) {
                    return Poll::Ready(Err(error));
                }
                if let Mode::Deflated(deflated) = &mut this.mode {
                    deflated.w_in.extend_from_slice(buf);
                }
                if let Err(error) = this.process_outbound() {
                    return Poll::Ready(Err(error.into_io()));
                }
                if let Poll::Ready(Err(error)) = this.poll_write_outbound(cx) {
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(Ok(buf.len()))
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match &mut this.mode {
            Mode::PassThrough { .. } => Pin::new(&mut this.inner).poll_flush(cx),
            Mode::Deflated(_) => match this.finish_outbound(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            },
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match &mut this.mode {
            Mode::PassThrough { .. } => Pin::new(&mut this.inner).poll_shutdown(cx),
            Mode::Deflated(_) => match this.finish_outbound(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(cx),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::Offer;

    fn limits() -> Limits {
        Limits::new(1024 * 1024, 1024 * 1024)
    }

    fn pmd(header: &str) -> PerMessageDeflate {
        crate::extension::negotiate(Offer::CHROMIUM, &[header])
            .unwrap()
            .unwrap()
    }

    #[test]
    fn frame_headers_bound_the_declared_length_before_buffering() {
        let bytes = vec![0x81, 126, 0x10, 0x00];
        assert_eq!(
            parse_header(&bytes, 1024, false),
            Err(AdapterError::FrameTooLarge)
        );
        let huge = vec![0x82, 127, 0x40, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00];
        assert_eq!(
            parse_header(&huge, 1024, false),
            Err(AdapterError::FrameTooLarge)
        );
    }

    #[test]
    fn reserved_bits_are_typed_failures() {
        assert_eq!(
            parse_header(&[0x91, 0x00], 1024, false),
            Err(AdapterError::InvalidReservedBits)
        );
        assert_eq!(
            parse_header(&[0xa1, 0x00], 1024, false),
            Err(AdapterError::InvalidReservedBits)
        );
        let mut masked = vec![0x81, 0x85, 1, 2, 3, 4];
        masked.extend_from_slice(b"hello");
        assert_eq!(
            parse_header(&masked, 1024, false),
            Err(AdapterError::MaskedServerFrame)
        );
    }

    #[test]
    fn control_frames_must_be_final_short_and_uncompressed() {
        assert_eq!(
            parse_header(&[0x09, 0x00], 1024, false),
            Err(AdapterError::InvalidControlFrame)
        );
        assert_eq!(
            parse_header(&[0x89, 0x7e, 0x00, 0x7e], 1024, false),
            Err(AdapterError::InvalidControlFrame)
        );
    }

    #[test]
    fn unknown_opcodes_are_rejected() {
        assert_eq!(
            parse_header(&[0x83, 0x00], 1024, false),
            Err(AdapterError::InvalidOpcode)
        );
    }

    #[test]
    fn masked_client_frames_expose_their_mask() {
        let mut bytes = vec![0x81, 0x85, 1, 2, 3, 4];
        bytes.extend_from_slice(&[b'h' ^ 1, b'e' ^ 2, b'l' ^ 3, b'l' ^ 4, b'o' ^ 1]);
        let header = parse_header(&bytes, 1024, true).unwrap().unwrap();
        assert_eq!(header.mask, Some([1, 2, 3, 4]));
        assert_eq!(header.payload_len, 5);
        assert_eq!(header.header_len, 6);
    }

    #[test]
    fn the_adapter_reports_the_negotiated_parameters() {
        let stream = CompressingStream::new(Vec::<u8>::new(), None, limits());
        assert_eq!(stream.negotiated(), None);
        let stream = CompressingStream::new(
            Vec::<u8>::new(),
            Some(pmd("permessage-deflate; server_no_context_takeover")),
            limits(),
        );
        assert_eq!(
            stream.negotiated(),
            Some(PerMessageDeflate {
                server_no_context_takeover: true,
                ..PerMessageDeflate::default()
            })
        );
    }

    #[test]
    fn size_limit_classification_matches_the_gateway_mapping() {
        let error = WsError::Io(AdapterError::MessageTooLarge.into_io());
        assert!(is_size_limit(&error));
        assert_eq!(classify(&error), Some(AdapterError::MessageTooLarge));
        let error = WsError::Io(AdapterError::CorruptDeflate.into_io());
        assert!(!is_size_limit(&error));
        assert_eq!(classify(&error), Some(AdapterError::CorruptDeflate));
    }
}
