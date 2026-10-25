//! Test-only server side of a WebSocket connection with optional
//! permessage-deflate.
//!
//! It speaks raw RFC 6455 frames (the client side under test masks them) and
//! RFC 7692 compression, so the tests can send compressed, fragmented and
//! deliberately malformed streams that no ordinary `tungstenite` server can
//! produce.

use std::io;

use flate2::{Compress, Compression, Decompress};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::extension::{self, OFFER, Offer, PerMessageDeflate};
use crate::stream::{AdapterError, deflate_message, inflate_raw};

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const READ_CHUNK_BYTES: usize = 4096;
const SYNC_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

/// One raw WebSocket frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub rsv1: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// A scripted server peer over any stream.
pub struct Peer<S> {
    io: S,
    pmd: Option<PerMessageDeflate>,
    deflate: Option<Compress>,
    inflate: Option<Decompress>,
    chunk: Box<[u8]>,
    preloaded: Vec<u8>,
    preloaded_pos: usize,
    control: Vec<Frame>,
}

impl<S> Peer<S> {
    /// A peer over an already-connected stream; `pmd` must match the
    /// negotiation the client performed.
    pub fn new(io: S, pmd: Option<PerMessageDeflate>) -> Self {
        Self::with_preloaded(io, pmd, Vec::new())
    }

    fn with_preloaded(io: S, pmd: Option<PerMessageDeflate>, preloaded: Vec<u8>) -> Self {
        let deflate = pmd.map(|_| Compress::new(Compression::default(), false));
        let inflate = pmd.map(|_| Decompress::new(false));
        Self {
            io,
            pmd,
            deflate,
            inflate,
            chunk: vec![0; 32 * 1024].into_boxed_slice(),
            preloaded,
            preloaded_pos: 0,
            control: Vec::new(),
        }
    }

    pub fn negotiated(&self) -> Option<PerMessageDeflate> {
        self.pmd
    }

    /// Control frames seen while assembling a data message, in order.
    pub fn control(&self) -> &[Frame] {
        &self.control
    }

    pub fn take_control(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.control)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Peer<S> {
    /// Writes one unmasked server frame.
    pub async fn send_frame(
        &mut self,
        fin: bool,
        rsv1: bool,
        opcode: u8,
        payload: &[u8],
    ) -> io::Result<()> {
        let mut header = Vec::with_capacity(10);
        header.push((if fin { 0x80 } else { 0 }) | (if rsv1 { 0x40 } else { 0 }) | opcode);
        append_length(&mut header, payload.len());
        self.io.write_all(&header).await?;
        self.io.write_all(payload).await?;
        self.io.flush().await
    }

    /// Writes bytes without framing them: used to build deliberately invalid
    /// frames in tests.
    pub async fn send_raw(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.io.write_all(bytes).await?;
        self.io.flush().await
    }

    pub async fn send_text(&mut self, text: &str) -> io::Result<()> {
        self.send_frame(true, false, 0x1, text.as_bytes()).await
    }

    pub async fn send_binary(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.send_frame(true, false, 0x2, bytes).await
    }

    pub async fn send_ping(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send_frame(true, false, 0x9, payload).await
    }

    pub async fn send_pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.send_frame(true, false, 0xA, payload).await
    }

    pub async fn send_close(&mut self, code: u16) -> io::Result<()> {
        self.send_frame(true, false, 0x8, &code.to_be_bytes()).await
    }

    /// Compresses one message with the peer's deflate context, applying
    /// `server_no_context_takeover`.
    pub fn compress_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, AdapterError> {
        let Some(pmd) = self.pmd else {
            return Ok(payload.to_vec());
        };
        let deflate = self.deflate.as_mut().ok_or(AdapterError::CorruptDeflate)?;
        let mut out = Vec::new();
        deflate_message(deflate, payload, &mut out, &mut self.chunk[..], usize::MAX)?;
        if pmd.server_no_context_takeover {
            deflate.reset();
        }
        Ok(out)
    }

    /// Sends one compressed message, fragmented into frames of at most
    /// `fragment_size` payload bytes (RSV1 only on the first).
    pub async fn send_compressed(
        &mut self,
        opcode: u8,
        payload: &[u8],
        fragment_size: usize,
    ) -> io::Result<()> {
        let compressed = self
            .compress_message(payload)
            .map_err(|_| io::Error::other("compression failed"))?;
        let size = fragment_size.max(1);
        let mut offset = 0;
        let mut first = true;
        loop {
            let end = (offset + size).min(compressed.len());
            let fin = end == compressed.len();
            let frame_opcode = if first { opcode } else { 0x0 };
            self.send_frame(fin, first, frame_opcode, &compressed[offset..end])
                .await?;
            first = false;
            if fin {
                break;
            }
            offset = end;
        }
        Ok(())
    }

    /// Reads the next client frame. `None` means the client closed the stream.
    pub async fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        let mut first = [0u8; 2];
        if !self.read_exact(&mut first).await? {
            return Ok(None);
        }
        let fin = first[0] & 0x80 != 0;
        let rsv1 = first[0] & 0x40 != 0;
        let opcode = first[0] & 0x0f;
        let masked = first[1] & 0x80 != 0;
        let length = (first[1] & 0x7f) as u64;
        let payload_len = match length {
            126 => {
                let mut bytes = [0u8; 2];
                self.read_required(&mut bytes).await?;
                u16::from_be_bytes(bytes) as usize
            }
            127 => {
                let mut bytes = [0u8; 8];
                self.read_required(&mut bytes).await?;
                let length = u64::from_be_bytes(bytes);
                if length > usize::MAX as u64 {
                    return Err(io::Error::other("frame too large"));
                }
                length as usize
            }
            length => length as usize,
        };
        let mask = if masked {
            let mut mask = [0u8; 4];
            self.read_required(&mut mask).await?;
            Some(mask)
        } else {
            None
        };
        let mut payload = vec![0; payload_len];
        self.read_required(&mut payload).await?;
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index & 3];
            }
        }
        Ok(Some(Frame {
            fin,
            rsv1,
            opcode,
            payload,
        }))
    }

    /// Reads one complete data message, inflating it when RSV1 was set.
    /// Control frames are collected in [`Peer::control`] and skipped.
    pub async fn recv_message(&mut self) -> io::Result<Option<(u8, Vec<u8>)>> {
        let mut message: Option<(u8, bool, Vec<u8>)> = None;
        loop {
            let Some(frame) = self.next_frame().await? else {
                return Ok(None);
            };
            if frame.opcode >= 0x8 {
                self.control.push(frame);
                continue;
            }
            match frame.opcode {
                0x0 => {
                    let Some((opcode, compressed, mut payload)) = message.take() else {
                        return Err(io::Error::other("unexpected continuation"));
                    };
                    payload.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return self.finish_message(opcode, compressed, payload).map(Some);
                    }
                    message = Some((opcode, compressed, payload));
                }
                0x1 | 0x2 => {
                    if message.is_some() {
                        return Err(io::Error::other("new message inside a fragmented one"));
                    }
                    if frame.fin {
                        return self
                            .finish_message(frame.opcode, frame.rsv1, frame.payload)
                            .map(Some);
                    }
                    message = Some((frame.opcode, frame.rsv1, frame.payload));
                }
                _ => return Err(io::Error::other("unknown opcode")),
            }
        }
    }

    fn finish_message(
        &mut self,
        opcode: u8,
        compressed: bool,
        payload: Vec<u8>,
    ) -> io::Result<(u8, Vec<u8>)> {
        if !compressed {
            return Ok((opcode, payload));
        }
        let Some(pmd) = self.pmd else {
            return Err(io::Error::other("compressed message without negotiation"));
        };
        let inflate = self
            .inflate
            .as_mut()
            .ok_or_else(|| io::Error::other("no inflater"))?;
        let mut input = payload;
        input.extend_from_slice(&SYNC_SUFFIX);
        let mut out = Vec::new();
        inflate_raw(inflate, &input, &mut out, &mut self.chunk[..], usize::MAX)
            .map_err(|_| io::Error::other("invalid compressed message"))?;
        if pmd.client_no_context_takeover {
            inflate.reset(false);
        }
        Ok((opcode, out))
    }

    async fn read_required(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let from_preload = self
            .preloaded
            .len()
            .saturating_sub(self.preloaded_pos)
            .min(buf.len());
        if from_preload > 0 {
            buf[..from_preload].copy_from_slice(
                &self.preloaded[self.preloaded_pos..self.preloaded_pos + from_preload],
            );
            self.preloaded_pos += from_preload;
        }
        let rest = &mut buf[from_preload..];
        if !rest.is_empty() {
            self.io.read_exact(rest).await?;
        }
        Ok(())
    }

    async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        if self.preloaded_pos < self.preloaded.len() {
            self.read_required(buf).await?;
            return Ok(true);
        }
        let mut filled = 0;
        while filled < buf.len() {
            let read = self.io.read(&mut buf[filled..]).await?;
            if read == 0 {
                if filled == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated frame",
                ));
            }
            filled += read;
        }
        Ok(true)
    }
}

/// Reads one opening-handshake request and returns its text.
pub async fn read_request<S: AsyncRead + Unpin>(io: &mut S) -> io::Result<String> {
    let mut head = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    loop {
        if find(&head, b"\r\n\r\n").is_some() {
            break;
        }
        if head.len() >= MAX_REQUEST_BYTES {
            return Err(io::Error::other("request too long"));
        }
        let read = io.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "closed before the request",
            ));
        }
        head.extend_from_slice(&chunk[..read]);
    }
    String::from_utf8(head).map_err(|_| io::Error::other("request is not UTF-8"))
}

/// Answers a valid request with 101 and an optional extension.
pub async fn write_accept<S: AsyncWrite + Unpin>(
    io: &mut S,
    request: &str,
    extension: Option<&str>,
) -> io::Result<()> {
    let key = request_header(request, "sec-websocket-key")
        .ok_or_else(|| io::Error::other("no Sec-WebSocket-Key"))?;
    let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
    write_response(
        io,
        "HTTP/1.1 101 Switching Protocols",
        &[
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Accept", accept.as_str()),
        ],
        extension,
    )
    .await
}

/// Writes an arbitrary response header plus the extension header.
pub async fn write_response<S: AsyncWrite + Unpin>(
    io: &mut S,
    status: &str,
    headers: &[(&str, &str)],
    extension: Option<&str>,
) -> io::Result<()> {
    let mut response = String::new();
    response.push_str(status);
    response.push_str("\r\n");
    for (name, value) in headers {
        response.push_str(name);
        response.push_str(": ");
        response.push_str(value);
        response.push_str("\r\n");
    }
    if let Some(extension) = extension {
        response.push_str("Sec-WebSocket-Extensions: ");
        response.push_str(extension);
        response.push_str("\r\n");
    }
    response.push_str("\r\n");
    io.write_all(response.as_bytes()).await?;
    io.flush().await
}

/// Accepts one handshake and returns the peer plus the request text.
pub async fn accept<S: AsyncRead + AsyncWrite + Unpin>(
    mut io: S,
    extension: Option<&str>,
) -> io::Result<(Peer<S>, String)> {
    let request = read_request(&mut io).await?;
    if request_header(&request, "sec-websocket-extensions") != Some(OFFER) {
        return Err(io::Error::other("client did not offer permessage-deflate"));
    }
    write_accept(&mut io, &request, extension).await?;
    let pmd = match extension {
        None => None,
        Some(header) => {
            let offer = Offer {
                client_max_window_bits: true,
                server_max_window_bits: true,
            };
            match extension::negotiate(offer, &[header]) {
                Ok(pmd) => pmd,
                Err(_) => return Err(io::Error::other("invalid extension response")),
            }
        }
    };
    Ok((Peer::with_preloaded(io, pmd, Vec::new()), request))
}

fn request_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request
        .split("\r\n")
        .filter_map(|line| line.split_once(':'))
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

/// The `Sec-WebSocket-Key` of a request, for tests that build a custom
/// response.
pub fn request_key(request: &str) -> Option<&str> {
    request_header(request, "sec-websocket-key")
}

fn append_length(out: &mut Vec<u8>, length: usize) {
    if length < 126 {
        out.push(length as u8);
    } else if length <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(length as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(length as u64).to_be_bytes());
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// A raw duplex pair: the client socket under test and its server peer.
pub async fn pair(
    pmd: Option<PerMessageDeflate>,
) -> (tokio::io::DuplexStream, Peer<tokio::io::DuplexStream>) {
    let (client, server) = tokio::io::duplex(2 * 1024 * 1024);
    (client, Peer::new(server, pmd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn a_peer_writes_and_reads_masked_frames() {
        let (mut client, mut peer) = pair(None).await;
        peer.send_text("hello").await.unwrap();
        let mut header = [0u8; 2];
        client.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [0x81, 5]);
        let mut payload = [0u8; 5];
        client.read_exact(&mut payload).await.unwrap();
        assert_eq!(&payload, b"hello");

        let mut frame = vec![0x81, 0x85, 1, 2, 3, 4];
        frame.extend_from_slice(&[b'w' ^ 1, b'o' ^ 2, b'r' ^ 3, b'l' ^ 4, b'd' ^ 1]);
        client.write_all(&frame).await.unwrap();
        let received = peer.next_frame().await.unwrap().unwrap();
        assert!(received.fin);
        assert_eq!(received.opcode, 0x1);
        assert_eq!(received.payload, b"world");
    }

    #[tokio::test]
    async fn compressed_fragments_include_rsv1_only_on_the_first_frame() {
        let pmd = extension::negotiate(
            Offer::CHROMIUM,
            &["permessage-deflate; server_no_context_takeover"],
        )
        .unwrap()
        .unwrap();
        let (mut client, mut peer) = pair(Some(pmd)).await;
        peer.send_compressed(0x1, b"fragmented text", 3)
            .await
            .unwrap();
        let mut frames = Vec::new();
        loop {
            let mut first = [0u8; 2];
            client.read_exact(&mut first).await.unwrap();
            assert_eq!(first[1] & 0x80, 0);
            let len = (first[1] & 0x7f) as usize;
            let mut payload = vec![0u8; len];
            client.read_exact(&mut payload).await.unwrap();
            frames.push((first[0], payload));
            if first[0] & 0x80 != 0 {
                break;
            }
        }
        assert!(frames.len() > 1);
        assert_eq!(frames[0].0 & 0x40, 0x40);
        for frame in &frames[1..] {
            assert_eq!(frame.0 & 0x40, 0);
            assert_eq!(frame.0 & 0x0f, 0);
        }
    }
}
