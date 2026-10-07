//! Gateway transport compression (SPEC §4.1): one persistent zlib context per
//! connection, fed WebSocket message payloads until the `Z_SYNC_FLUSH` suffix.
//!
//! zstd-stream is deliberately absent. The enum stays internal and has exactly
//! the one shipped variant.

use flate2::{Decompress, FlushDecompress, Status};

/// The only compression scheme: its `compress=` query value selects the
/// decoder in [`Inflater`]. Identify's legacy payload-compression flag stays
/// false; enabling both schemes is not allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Compression {
    ZlibStream,
}

impl Compression {
    pub(crate) const fn query_value(self) -> &'static str {
        match self {
            Self::ZlibStream => "zlib-stream",
        }
    }
}

/// Client safety ceiling for one decompressed event (and for the compressed
/// bytes accumulated toward it). Not a claim about Discord's inbound limit.
pub(crate) const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;
/// Scratch capacity kept between events; anything larger (READY) is released
/// when the next event starts.
const RETAINED_SCRATCH_BYTES: usize = 256 * 1024;
const CHUNK_BYTES: usize = 32 * 1024;
const SYNC_SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InflateError {
    /// The compressed accumulation or the decompressed event exceeded
    /// [`MAX_EVENT_BYTES`].
    EventTooLarge,
    /// Not a valid zlib stream (or it ended, which a live stream never does).
    Corrupt,
}

/// Decoder for one connection. Create a new one for every new connection;
/// the dictionary must never be shared or reset mid-connection.
pub(crate) struct Inflater {
    inflater: Decompress,
    /// Compressed bytes of the event being assembled.
    pending: Vec<u8>,
    /// The last complete decompressed event.
    message: Vec<u8>,
    chunk: Box<[u8]>,
}

impl Inflater {
    pub(crate) fn new() -> Self {
        Self {
            inflater: Decompress::new(true),
            pending: Vec::new(),
            message: Vec::new(),
            chunk: vec![0; CHUNK_BYTES].into_boxed_slice(),
        }
    }

    /// Appends one WebSocket message payload. Returns `true` when the
    /// accumulated bytes end with the sync-flush suffix and a complete event is
    /// available from [`message`](Self::message). The suffix is looked for at
    /// the end of the accumulation, so a message boundary may fall anywhere,
    /// including inside the suffix itself.
    pub(crate) fn push_compressed(&mut self, bytes: &[u8]) -> Result<bool, InflateError> {
        if bytes.len() > MAX_EVENT_BYTES.saturating_sub(self.pending.len()) {
            return Err(InflateError::EventTooLarge);
        }
        self.pending.extend_from_slice(bytes);
        if !self.pending.ends_with(&SYNC_SUFFIX) {
            return Ok(false);
        }
        self.inflate()?;
        self.pending.clear();
        if self.pending.capacity() > RETAINED_SCRATCH_BYTES {
            self.pending = Vec::new();
        }
        Ok(true)
    }

    /// Makes an uncompressed text frame the current event.
    pub(crate) fn set_plain(&mut self, bytes: &[u8]) -> Result<(), InflateError> {
        if bytes.len() > MAX_EVENT_BYTES {
            return Err(InflateError::EventTooLarge);
        }
        self.reset_message();
        self.message.extend_from_slice(bytes);
        Ok(())
    }

    /// The most recent complete event. Valid until the next push.
    pub(crate) fn message(&self) -> &[u8] {
        &self.message
    }

    /// Call once the current event has been handled: a huge event (READY) must
    /// not keep its buffer alive until the next one arrives.
    pub(crate) fn finish_message(&mut self) {
        self.reset_message();
    }

    fn reset_message(&mut self) {
        self.message.clear();
        if self.message.capacity() > RETAINED_SCRATCH_BYTES {
            self.message = Vec::new();
        }
    }

    fn inflate(&mut self) -> Result<(), InflateError> {
        self.reset_message();
        let mut input: &[u8] = &self.pending;
        loop {
            let (before_in, before_out) = (self.inflater.total_in(), self.inflater.total_out());
            let status = self
                .inflater
                .decompress(input, &mut self.chunk, FlushDecompress::Sync)
                .map_err(|_| InflateError::Corrupt)?;
            let consumed = (self.inflater.total_in() - before_in) as usize;
            let produced = (self.inflater.total_out() - before_out) as usize;
            input = &input[consumed..];
            if produced > MAX_EVENT_BYTES - self.message.len() {
                return Err(InflateError::EventTooLarge);
            }
            self.message.extend_from_slice(&self.chunk[..produced]);
            if status == Status::StreamEnd {
                return Err(InflateError::Corrupt);
            }
            if input.is_empty() && produced < self.chunk.len() {
                return Ok(());
            }
            if consumed == 0 && produced == 0 {
                // Unconsumed input with no progress: a corrupt stream.
                return Err(InflateError::Corrupt);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use flate2::{Compress, Compression as Level, FlushCompress};

    use super::SYNC_SUFFIX;

    /// The server side of a zlib-stream connection: one persistent compressor,
    /// each event ending in a sync flush.
    pub(crate) struct ServerCompressor(Compress);

    impl ServerCompressor {
        pub(crate) fn new() -> Self {
            Self(Compress::new(Level::default(), true))
        }

        pub(crate) fn event(&mut self, json: &[u8]) -> Vec<u8> {
            // Comfortably above deflate's worst-case expansion plus the flush marker.
            let mut out = Vec::with_capacity(json.len() + json.len() / 8 + 256);
            let before = self.0.total_in();
            self.0
                .compress_vec(json, &mut out, FlushCompress::Sync)
                .unwrap();
            assert_eq!((self.0.total_in() - before) as usize, json.len());
            assert!(out.ends_with(&SYNC_SUFFIX));
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::ServerCompressor;
    use super::*;

    fn feed(inflater: &mut Inflater, bytes: &[u8], sizes: &[usize]) -> Vec<Vec<u8>> {
        // Splits `bytes` at the given chunk sizes (repeating the last), returning
        // every complete event produced.
        let mut events = Vec::new();
        let mut rest = bytes;
        let mut index = 0;
        while !rest.is_empty() {
            let size = sizes[index.min(sizes.len() - 1)].min(rest.len());
            index += 1;
            let (head, tail) = rest.split_at(size);
            rest = tail;
            if inflater.push_compressed(head).unwrap() {
                events.push(inflater.message().to_vec());
            }
        }
        events
    }

    #[test]
    fn query_value_is_the_documented_scheme() {
        assert_eq!(Compression::ZlibStream.query_value(), "zlib-stream");
    }

    #[test]
    fn dictionary_is_retained_across_events() {
        let mut server = ServerCompressor::new();
        let first = server.event(br#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
        let second = server.event(br#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
        // The repeated payload compresses to far fewer bytes only because the
        // second event references the first through the shared window.
        assert!(second.len() < first.len() / 2);
        let mut inflater = Inflater::new();
        assert!(inflater.push_compressed(&first).unwrap());
        assert_eq!(
            inflater.message(),
            br#"{"op":10,"d":{"heartbeat_interval":41250}}"#
        );
        assert!(inflater.push_compressed(&second).unwrap());
        assert_eq!(
            inflater.message(),
            br#"{"op":10,"d":{"heartbeat_interval":41250}}"#
        );
    }

    #[test]
    fn events_fragmented_at_every_boundary_reassemble() {
        let payloads: Vec<Vec<u8>> = (0..6)
            .map(|n| {
                format!(
                    r#"{{"op":0,"s":{n},"t":"MESSAGE_CREATE","d":{{"content":"{}"}}}}"#,
                    "x".repeat(n * 700)
                )
                .into_bytes()
            })
            .collect();
        for chunk in [1usize, 2, 3, 5, 7, 64, 1000] {
            // A WebSocket message never spans two events, only the other way
            // round: fragment each event separately, sharing one dictionary.
            let mut server = ServerCompressor::new();
            let mut inflater = Inflater::new();
            let mut events = Vec::new();
            for payload in &payloads {
                events.extend(feed(&mut inflater, &server.event(payload), &[chunk]));
            }
            assert_eq!(events, payloads, "chunk size {chunk}");
        }
    }

    #[test]
    fn suffix_split_across_messages_still_completes_the_event() {
        let mut server = ServerCompressor::new();
        let event = server.event(br#"{"op":11,"d":null}"#);
        let (head, tail) = event.split_at(event.len() - 2);
        let mut inflater = Inflater::new();
        assert!(!inflater.push_compressed(head).unwrap());
        assert!(inflater.push_compressed(tail).unwrap());
        assert_eq!(inflater.message(), br#"{"op":11,"d":null}"#);
    }

    #[test]
    fn partial_data_without_the_suffix_yields_nothing() {
        let mut server = ServerCompressor::new();
        let event = server.event(br#"{"op":11,"d":null}"#);
        let mut inflater = Inflater::new();
        assert!(!inflater.push_compressed(&event[..event.len() - 4]).unwrap());
    }

    #[test]
    fn new_connection_requires_a_new_dictionary() {
        let mut server = ServerCompressor::new();
        let _ = server.event(br#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
        let second = server.event(br#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
        // A decoder that never saw the first event cannot decode the second.
        let mut fresh = Inflater::new();
        assert_eq!(fresh.push_compressed(&second), Err(InflateError::Corrupt));
    }

    #[test]
    fn garbage_is_corrupt_not_a_panic() {
        let mut inflater = Inflater::new();
        let mut garbage = vec![0x12u8; 64];
        garbage.extend_from_slice(&SYNC_SUFFIX);
        assert_eq!(
            inflater.push_compressed(&garbage),
            Err(InflateError::Corrupt)
        );
    }

    #[test]
    fn decompressed_event_ceiling_stops_a_bomb_before_allocating_it() {
        let mut server = ServerCompressor::new();
        // 20 MiB of one byte compresses to a few tens of KiB.
        let mut json = br#"{"d":""#.to_vec();
        json.resize(json.len() + MAX_EVENT_BYTES + 4 * 1024 * 1024, b'a');
        json.extend_from_slice(br#""}"#);
        let bomb = server.event(&json);
        assert!(bomb.len() < 128 * 1024, "fixture must be small on the wire");
        let mut inflater = Inflater::new();
        assert_eq!(
            inflater.push_compressed(&bomb),
            Err(InflateError::EventTooLarge)
        );
        assert!(inflater.message().len() <= MAX_EVENT_BYTES);
    }

    #[test]
    fn compressed_accumulation_is_bounded_too() {
        let mut inflater = Inflater::new();
        let block = vec![0u8; 4 * 1024 * 1024];
        for _ in 0..4 {
            assert_eq!(inflater.push_compressed(&block), Ok(false));
        }
        assert_eq!(
            inflater.push_compressed(&[1]),
            Err(InflateError::EventTooLarge)
        );
    }

    #[test]
    fn large_scratch_is_released_before_the_next_event() {
        let mut server = ServerCompressor::new();
        let mut big = br#"{"d":""#.to_vec();
        big.resize(big.len() + 3 * 1024 * 1024, b'a');
        big.extend_from_slice(br#""}"#);
        let mut inflater = Inflater::new();
        assert!(inflater.push_compressed(&server.event(&big)).unwrap());
        assert!(inflater.message().len() > 3 * 1024 * 1024);
        assert!(inflater.message.capacity() > RETAINED_SCRATCH_BYTES);
        assert!(
            inflater
                .push_compressed(&server.event(br#"{"op":11}"#))
                .unwrap()
        );
        assert_eq!(inflater.message(), br#"{"op":11}"#);
        assert!(inflater.message.capacity() <= RETAINED_SCRATCH_BYTES);
        assert!(inflater.pending.capacity() <= RETAINED_SCRATCH_BYTES);

        // The connection releases the scratch as soon as an event is handled,
        // not only when the next one starts.
        let mut server = ServerCompressor::new();
        let mut inflater = Inflater::new();
        assert!(inflater.push_compressed(&server.event(&big)).unwrap());
        inflater.finish_message();
        assert!(inflater.message().is_empty());
        assert!(inflater.message.capacity() <= RETAINED_SCRATCH_BYTES);
    }

    #[test]
    fn plain_text_frames_share_the_ceiling() {
        let mut inflater = Inflater::new();
        inflater.set_plain(b"{\"op\":11}").unwrap();
        assert_eq!(inflater.message(), b"{\"op\":11}");
        assert_eq!(
            inflater.set_plain(&vec![b' '; MAX_EVENT_BYTES + 1]),
            Err(InflateError::EventTooLarge)
        );
    }
}
