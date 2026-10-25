//! Controls only sockets created by this crate. No packet interception or
//! global settings. Safe socket2 APIs keep platform calls outside Rust code.
use std::io;
use std::net::Shutdown;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use socket2::SockRef;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use crate::NetworkError;
use crate::tls::{FakePrefixFactory, PrefixControl, PrefixFuture};

pub(crate) type Counters = Arc<Mutex<(usize, usize)>>;

pub(crate) struct NativeIo {
    shared: Arc<Control>,
}
struct Control {
    stream: TcpStream,
    counters: Option<Counters>,
    inbound: usize,
    total: usize,
    factory: Option<Arc<dyn FakePrefixFactory>>,
}

impl NativeIo {
    pub(crate) fn new(stream: TcpStream) -> Self {
        Self {
            shared: Arc::new(Control {
                stream,
                counters: None,
                inbound: usize::MAX,
                total: usize::MAX,
                factory: None,
            }),
        }
    }
    pub(crate) fn bounded(
        stream: TcpStream,
        counters: Counters,
        inbound: usize,
        total: usize,
    ) -> Self {
        Self {
            shared: Arc::new(Control {
                stream,
                counters: Some(counters),
                inbound,
                total,
                factory: None,
            }),
        }
    }
    pub(crate) fn control(&self) -> Arc<dyn PrefixControl> {
        self.shared.clone()
    }
    pub(crate) fn with_factory(mut self, factory: Option<Arc<dyn FakePrefixFactory>>) -> Self {
        if let Some(control) = Arc::get_mut(&mut self.shared) {
            control.factory = factory;
        }
        self
    }
}

impl Control {
    fn counts(&self) -> (usize, usize) {
        self.counters
            .as_ref()
            .map_or((0, 0), |c| *c.lock().unwrap_or_else(|p| p.into_inner()))
    }
    fn remaining(&self) -> usize {
        let (sent, received) = self.counts();
        self.total.saturating_sub(sent.saturating_add(received))
    }
    fn sent(&self, length: usize) {
        if let Some(counters) = &self.counters {
            counters.lock().unwrap_or_else(|p| p.into_inner()).0 += length;
        }
    }
    fn send(&self, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let remaining = self.remaining();
        if remaining == 0 {
            return Poll::Ready(Err(io::Error::other(NetworkError::BudgetExhausted)));
        }
        loop {
            std::task::ready!(self.stream.poll_write_ready(cx))?;
            let result = self.stream.try_write(&bytes[..bytes.len().min(remaining)]);
            match result {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(length) => {
                    self.sent(length);
                    return Poll::Ready(Ok(length));
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

impl PrefixControl for Control {
    fn start_fake(
        &self,
        original: Vec<u8>,
        fake: Vec<u8>,
        strategy: crate::tls::Strategy,
    ) -> io::Result<PrefixFuture> {
        // Reserve every fake packet and IP/TCP header, including the split
        // real prefix. Kernel retransmission remains separate accounting.
        let charge = original.len().saturating_add(8192);
        if charge > self.remaining() {
            return Err(io::Error::other(NetworkError::BudgetExhausted));
        }
        let factory = self.factory.as_ref().ok_or(io::ErrorKind::Unsupported)?;
        let socket: std::net::TcpStream = SockRef::from(&self.stream).try_clone()?.into();
        let operation = factory.start(socket, original, fake, strategy)?;
        self.sent(charge);
        Ok(operation)
    }
    fn close(&self) {
        let _ = SockRef::from(&self.stream).shutdown(Shutdown::Both);
    }
}

impl AsyncRead for NativeIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let limit = self
            .shared
            .remaining()
            .min(self.shared.inbound.saturating_sub(self.shared.counts().1));
        if limit == 0 {
            return Poll::Ready(Err(io::Error::other(NetworkError::BudgetExhausted)));
        }
        let length = buffer.remaining().min(limit);
        loop {
            std::task::ready!(self.shared.stream.poll_read_ready(cx))?;
            match self
                .shared
                .stream
                .try_read(&mut buffer.initialize_unfilled()[..length])
            {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Ok(length) => {
                    buffer.advance(length);
                    if let Some(counters) = &self.shared.counters {
                        counters.lock().unwrap_or_else(|p| p.into_inner()).1 += length;
                    }
                    return Poll::Ready(Ok(()));
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}
impl AsyncWrite for NativeIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.shared.send(cx, bytes)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(SockRef::from(&self.shared.stream).shutdown(Shutdown::Write))
    }
}
