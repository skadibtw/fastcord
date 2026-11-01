//! One anonymous request's transport receipt. Never contains endpoints or secrets.
use crate::{
    selector::TlsProfile,
    settings::Mode,
    tls::{FakePrefixFactory, PrefixFuture, Strategy},
};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
pub struct TransportEvidence {
    pub mode: Mode,
    pub profile: TlsProfile,
    pub helper_available: bool,
    pub selected: Option<Strategy>,
    pub packet_attempted: bool,
    /// The helper completed its transformation successfully, not proof of service reachability.
    pub packet_completed: bool,
}
#[derive(Clone)]
pub(crate) struct Receipt(Arc<Mutex<TransportEvidence>>);
impl Receipt {
    pub fn new(mode: Mode, profile: TlsProfile, helper_available: bool) -> Self {
        Self(Arc::new(Mutex::new(TransportEvidence {
            mode,
            profile,
            helper_available,
            selected: None,
            packet_attempted: false,
            packet_completed: false,
        })))
    }
    pub fn snapshot(&self) -> TransportEvidence {
        *self.0.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub fn selected(&self, strategy: Strategy) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).selected = Some(strategy);
    }
    pub fn wrap(&self, inner: Arc<dyn FakePrefixFactory>) -> Arc<dyn FakePrefixFactory> {
        Arc::new(Observed {
            receipt: self.clone(),
            inner,
        })
    }
}
struct Observed {
    receipt: Receipt,
    inner: Arc<dyn FakePrefixFactory>,
}
impl FakePrefixFactory for Observed {
    fn available(&self) -> bool {
        self.inner.available()
    }
    fn strategy(&self) -> Strategy {
        self.inner.strategy()
    }
    fn start(
        &self,
        socket: std::net::TcpStream,
        original: Vec<u8>,
        fake: Vec<u8>,
        strategy: Strategy,
    ) -> std::io::Result<PrefixFuture> {
        self.receipt
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .packet_attempted = true;
        let future = self.inner.start(socket, original, fake, strategy)?;
        let receipt = self.receipt.clone();
        Ok(Box::pin(async move {
            let result = future.await;
            if result.is_ok() {
                receipt
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .packet_completed = true;
            }
            result
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Failed;
    impl FakePrefixFactory for Failed {
        fn start(
            &self,
            _: std::net::TcpStream,
            _: Vec<u8>,
            _: Vec<u8>,
            _: Strategy,
        ) -> std::io::Result<PrefixFuture> {
            Err(std::io::ErrorKind::ConnectionReset.into())
        }
    }
    #[test]
    fn failed_helper_attempt_is_distinct_from_completion_and_other_requests() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let receipt = Receipt::new(Mode::Auto, TlsProfile::Http, true);
        let other = Receipt::new(Mode::Auto, TlsProfile::Http, true);
        receipt.selected(Strategy::PacketFake);
        assert!(
            receipt
                .wrap(Arc::new(Failed))
                .start(socket, vec![], vec![], Strategy::PacketFake)
                .is_err()
        );
        assert!(receipt.snapshot().packet_attempted);
        assert!(!receipt.snapshot().packet_completed);
        assert!(!other.snapshot().packet_attempted);
        assert!(other.snapshot().selected.is_none());
    }
}
