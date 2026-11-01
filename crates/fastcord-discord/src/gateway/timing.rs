//! Opt-in, fixed-size startup evidence. Never retains bytes or remote identifiers.
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::time::Instant;

/// Process-local startup correlation carried with the bounded READY event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartupStamp {
    attempt: u64,
    started: Instant,
}
#[derive(Clone, Copy, Debug)]
pub enum StartupStage {
    ReducerApplied,
    SnapshotPublished,
    UiConsumed,
}
impl StartupStamp {
    pub fn record(self, stage: StartupStage) {
        self.record_at(stage, Instant::now());
    }
    pub fn record_at(self, stage: StartupStage, at: Instant) {
        eprintln!(
            "Gateway publication timing: attempt={} stage={stage:?} elapsed_us={}",
            self.attempt,
            at.saturating_duration_since(self.started).as_micros()
        );
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Copy, Debug)]
pub(crate) enum Stage {
    Discovery,
    Transport,
    Upgrade,
    Hello,
    StartupSent,
    ReadyQueued,
}
#[derive(Clone, Copy, Default, Debug)]
struct Counts {
    stages_ms: [Option<u64>; 6],
    reads: u64,
    bytes: u64,
    first_read_ms: Option<u64>,
    last_read_ms: Option<u64>,
    messages: u64,
    decoded: u64,
    decode_us: u64,
    inflate_chunks: u64,
}
#[derive(Clone)]
pub(crate) struct Timing(Arc<Inner>);
struct Inner {
    id: u64,
    started: Instant,
    counts: Mutex<Counts>,
    finalized: AtomicBool,
    network: fastcord_net::timing::ConnectTiming,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if !self.finalized.load(Ordering::Relaxed) {
            eprintln!(
                "Gateway timing: attempt={} elapsed_ms={} reason=cancelled_or_connect_failed {:?} transport={:?}",
                self.id,
                self.started.elapsed().as_millis(),
                self.counts.get_mut().unwrap_or_else(|p| p.into_inner()),
                self.network.snapshot()
            );
        }
    }
}

impl Timing {
    pub(crate) fn enabled() -> Option<Self> {
        (std::env::var_os("FASTCORD_GATEWAY_TIMING").is_some()).then(Self::new)
    }
    pub(crate) fn new() -> Self {
        Self(Arc::new(Inner {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            counts: Mutex::new(Counts::default()),
            finalized: AtomicBool::new(false),
            network: fastcord_net::timing::ConnectTiming::default(),
        }))
    }
    pub(crate) fn stamp(&self) -> StartupStamp {
        StartupStamp {
            attempt: self.0.id,
            started: self.0.started,
        }
    }
    pub(crate) fn network(&self) -> &fastcord_net::timing::ConnectTiming {
        &self.0.network
    }
    fn elapsed_ms(&self) -> u64 {
        self.0.started.elapsed().as_millis().min(u64::MAX as u128) as u64
    }
    pub(crate) fn stage(&self, stage: Stage) {
        self.0
            .counts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .stages_ms[stage as usize] = Some(self.elapsed_ms());
    }
    pub(crate) fn message(&self) {
        let mut c = self.0.counts.lock().unwrap_or_else(|p| p.into_inner());
        c.messages = c.messages.saturating_add(1);
    }
    pub(crate) fn decoded(&self, micros: u64) {
        let mut c = self.0.counts.lock().unwrap_or_else(|p| p.into_inner());
        c.decoded = c.decoded.saturating_add(1);
        c.decode_us = c.decode_us.saturating_add(micros);
    }
    fn processed(&self, micros: u64) {
        let mut c = self.0.counts.lock().unwrap_or_else(|p| p.into_inner());
        c.inflate_chunks = c.inflate_chunks.saturating_add(1);
        c.decode_us = c.decode_us.saturating_add(micros);
    }
    fn read(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let now = self.elapsed_ms();
        let mut c = self.0.counts.lock().unwrap_or_else(|p| p.into_inner());
        c.reads = c.reads.saturating_add(1);
        c.bytes = c.bytes.saturating_add(bytes as u64);
        c.first_read_ms.get_or_insert(now);
        c.last_read_ms = Some(now);
    }
    pub(crate) fn report(&self, reason: &'static str) {
        // At most READY and termination records; static categories only.
        if !matches!(reason, "ready_queued" | "resumed_queued") {
            self.0.finalized.store(true, Ordering::Relaxed);
        }
        let counts = *self.0.counts.lock().unwrap_or_else(|p| p.into_inner());
        eprintln!(
            "Gateway timing: attempt={} elapsed_ms={} reason={} {:?} transport={:?}",
            self.0.id,
            self.elapsed_ms(),
            reason,
            counts,
            self.0.network.snapshot()
        );
    }
}
pub(crate) fn measure_inflate<T>(timing: Option<&Timing>, operation: impl FnOnce() -> T) -> T {
    let Some(timing) = timing else {
        return operation();
    };
    let started = Instant::now();
    let result = operation();
    {
        timing.processed(started.elapsed().as_micros().min(u64::MAX as u128) as u64);
    }
    result
}

pub(crate) struct CountedIo<S> {
    pub inner: S,
    pub timing: Timing,
}
impl<S: AsyncRead + Unpin> AsyncRead for CountedIo<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.inner).poll_read(cx, buf);
        if matches!(result, Poll::Ready(Ok(()))) {
            self.timing.read(buf.filled().len() - before);
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for CountedIo<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    #[test]
    fn partial_and_rejected_inflate_chunks_are_counted_without_completed_messages() {
        use crate::gateway::compression::{Inflater, test_support::ServerCompressor};
        let mut compressor = ServerCompressor::new();
        let bytes = compressor.event(br#"{"op":11}"#);
        let timing = Timing::new();
        let mut inflater = Inflater::new();
        assert!(
            !measure_inflate(Some(&timing), || inflater
                .push_compressed(&bytes[..bytes.len() - 4]))
            .unwrap()
        );
        assert_eq!(timing.0.counts.lock().unwrap().inflate_chunks, 1);
        assert_eq!(timing.0.counts.lock().unwrap().decoded, 0);
        assert!(
            measure_inflate(Some(&timing), || inflater
                .push_compressed(&bytes[bytes.len() - 4..]))
            .unwrap()
        );
        let mut broken = Inflater::new();
        assert!(
            measure_inflate(Some(&timing), || broken
                .push_compressed(&[255, 255, 255, 255, 0, 0, 255, 255]))
            .is_err()
        );
        let c = timing.0.counts.lock().unwrap();
        assert_eq!(c.inflate_chunks, 3);
        assert_eq!(c.decoded, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn partial_frame_reads_remain_visible_when_the_remainder_stalls() {
        let (mut peer, io) = tokio::io::duplex(64);
        let timing = Timing::new();
        let io = CountedIo {
            inner: io,
            timing: timing.clone(),
        };
        // Binary frame advertises ten bytes, but only two arrive.
        peer.write_all(&[0x82, 10, 1, 2]).await.unwrap();
        let mut socket = tokio_tungstenite::WebSocketStream::from_raw_socket(
            io,
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(30), socket.next())
                .await
                .is_err()
        );
        let c = timing.0.counts.lock().unwrap();
        assert_eq!(c.bytes, 4);
        assert_eq!(c.reads, 1);
        assert_eq!(c.messages, 0);
        assert_eq!(c.first_read_ms, Some(0));
        assert_eq!(c.last_read_ms, Some(0));
    }
}
