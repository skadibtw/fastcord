//! QR login through Discord's remote-auth protocol (ADR 0006).
//!
//! One [`RemoteAuth`] is one attempt: it owns a fresh RSA key pair, one gateway
//! WebSocket, and nothing else. Dropping it (cancel, regenerate, window
//! teardown) aborts the attempt and drops every secret. Password login, MFA,
//! CAPTCHA solving, and account verification are deliberately absent: a
//! challenge from Discord is surfaced as [`RemoteAuthError::Captcha`] and the
//! flow stops.

mod crypto;
mod exchange;
mod protocol;
mod session;
mod transport;

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use protocol::RemoteUser;

use crate::UserToken;
use crypto::AttemptKey;
use exchange::HttpTicketExchange;

/// Events are small and the UI drains them as they arrive; a handful of slots
/// bounds memory while letting the driver finish a step before the UI polls.
const EVENT_QUEUE: usize = 8;
const QR_URL_PREFIX: &str = "https://discord.com/ra/";

/// The URL to render as a QR code. Redacted in `Debug`: whoever scans it
/// authorizes *their* account into this client, so it is not for logs.
#[derive(Clone, PartialEq, Eq)]
pub struct QrLink {
    url: String,
    expires_in: Duration,
}

impl QrLink {
    pub fn new(fingerprint: &str, expires_in: Duration) -> Self {
        Self {
            url: format!("{QR_URL_PREFIX}{fingerprint}"),
            expires_in,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.url
    }

    /// Time left on the server-defined session when the code became valid.
    pub fn expires_in(&self) -> Duration {
        self.expires_in
    }
}

impl fmt::Debug for QrLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QrLink")
            .field("expires_in", &self.expires_in)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub enum RemoteAuthEvent {
    /// The gateway accepted our key; show this QR code.
    Qr(QrLink),
    /// A phone scanned the code; its owner must now confirm there.
    PendingUser(RemoteUser),
    /// Terminal. Still unvalidated: the caller must run the normal
    /// `GET /users/@me` validation and storage path.
    Authorized(Arc<UserToken>),
    /// Terminal.
    Failed(RemoteAuthError),
}

impl fmt::Debug for RemoteAuthEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Qr(link) => f.debug_tuple("Qr").field(link).finish(),
            Self::PendingUser(user) => f.debug_tuple("PendingUser").field(user).finish(),
            Self::Authorized(_) => f.write_str("Authorized([REDACTED])"),
            Self::Failed(error) => f.debug_tuple("Failed").field(error).finish(),
        }
    }
}

/// Categorical failures only: gateway frames, tickets, and response bodies are
/// never copied into an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoteAuthError {
    /// Could not reach or handshake with the remote-auth gateway.
    Connect,
    KeyGeneration,
    /// The gateway sent something outside the documented protocol.
    Protocol,
    /// A payload could not be decrypted with this attempt's key.
    Crypto,
    FingerprintMismatch,
    /// The server-side session timed out (or its lifetime elapsed locally).
    Expired,
    CancelledOnPhone,
    /// The caller dropped the event receiver.
    Cancelled,
    /// The server stopped answering heartbeats.
    HeartbeatLost,
    /// The gateway closed the connection (optional close code).
    Closed(Option<u16>),
    /// Ticket exchange requires a CAPTCHA, which fastcord does not solve.
    Captcha,
    RateLimited,
    ExchangeNetwork,
    ExchangeRejected(u16),
}

impl RemoteAuthError {
    /// Whether generating a new QR code can plausibly help. A CAPTCHA demand
    /// would simply repeat, so it points at other login methods instead.
    pub fn can_regenerate(self) -> bool {
        !matches!(self, Self::Captcha | Self::Cancelled)
    }
}

impl fmt::Display for RemoteAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect => f.write_str("Could not connect to Discord's QR login service. Check your network and try again."),
            Self::KeyGeneration => f.write_str("Could not generate this login attempt's encryption key."),
            Self::Protocol => f.write_str("Discord's QR login service sent an unexpected response. Login stopped; use token login or try again."),
            Self::Crypto => f.write_str("Discord's QR login response could not be decrypted. Login stopped; try again."),
            Self::FingerprintMismatch => f.write_str("Discord's QR login service did not confirm this attempt's key. Login stopped for safety; try again."),
            Self::Expired => f.write_str("This QR code expired."),
            Self::CancelledOnPhone => f.write_str("Login was cancelled on the phone."),
            Self::Cancelled => f.write_str("QR login was cancelled."),
            Self::HeartbeatLost => f.write_str("The connection to Discord's QR login service stopped responding."),
            Self::Closed(_) => f.write_str("Discord's QR login service closed the connection."),
            Self::Captcha => f.write_str("Discord requires a CAPTCHA to finish this QR login. fastcord cannot solve CAPTCHAs and does not bypass them. Use token login (advanced) or log in with the official Discord client."),
            Self::RateLimited => f.write_str("Discord is rate limiting QR logins. Wait a few minutes before trying again."),
            Self::ExchangeNetwork => f.write_str("Could not reach Discord to finish the QR login. Try again."),
            Self::ExchangeRejected(status) => write!(f, "Discord rejected the QR login (HTTP {status}). Try a new QR code."),
        }
    }
}

impl std::error::Error for RemoteAuthError {}

/// A running attempt. Dropping it aborts the attempt immediately.
pub struct RemoteAuth {
    events: mpsc::Receiver<RemoteAuthEvent>,
    task: JoinHandle<()>,
}

impl fmt::Debug for RemoteAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteAuth")
    }
}

impl RemoteAuth {
    /// Starts a new attempt with a fresh key pair. Must be called inside a
    /// Tokio runtime. Nothing is sent until the gateway connection opens, which
    /// happens only because the caller (an explicit user action) asked for it.
    pub fn start() -> Self {
        let (events_tx, events) = mpsc::channel(EVENT_QUEUE);
        let task = tokio::spawn(async move {
            let event = match run(&events_tx).await {
                Ok(token) => RemoteAuthEvent::Authorized(Arc::new(token)),
                Err(RemoteAuthError::Cancelled) => return,
                Err(error) => RemoteAuthEvent::Failed(error),
            };
            let _ = events_tx.send(event).await;
        });
        Self { events, task }
    }

    /// `None` after the terminal event has been delivered.
    pub async fn next_event(&mut self) -> Option<RemoteAuthEvent> {
        self.events.recv().await
    }
}

impl Drop for RemoteAuth {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn run(events: &mpsc::Sender<RemoteAuthEvent>) -> Result<UserToken, RemoteAuthError> {
    let exchange = HttpTicketExchange::new()?;
    // Key generation overlaps the network handshake so `init` can be sent as
    // soon as the gateway's hello arrives.
    let (key, socket) = tokio::join!(
        tokio::task::spawn_blocking(AttemptKey::generate),
        transport::connect()
    );
    let key = key.map_err(|_| RemoteAuthError::KeyGeneration)??;
    session::drive(socket?, &key, &exchange, events).await
}

/// Manual checks against Discord's real, unauthenticated remote-auth endpoints
/// (open risk U6). They need only network access, never a credential:
/// `cargo test -p fastcord-discord live_ --locked -- --ignored --nocapture`.
#[cfg(test)]
mod live {
    use std::time::Duration;

    use exchange::TicketExchange;

    use super::*;

    #[tokio::test]
    #[ignore = "contacts Discord"]
    async fn live_gateway_issues_a_qr_code_and_keeps_the_session_alive() {
        let mut auth = RemoteAuth::start();
        let event = tokio::time::timeout(Duration::from_secs(30), auth.next_event())
            .await
            .expect("no event within 30 s")
            .expect("attempt ended without an event");
        let RemoteAuthEvent::Qr(link) = event else {
            panic!("expected a QR code, got {event:?}");
        };
        let fingerprint = link.as_str().strip_prefix(QR_URL_PREFIX).unwrap();
        assert_eq!(fingerprint.len(), 43);
        println!("server session lifetime: {:?}", link.expires_in());
        // Outlast two heartbeat intervals (about 41 s each): a missing ack would
        // end the attempt with `HeartbeatLost` at the second tick.
        match tokio::time::timeout(Duration::from_secs(95), auth.next_event()).await {
            Err(_) => println!("session still alive after 95 s: heartbeat acks work"),
            Ok(event) => panic!("session ended early: {event:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "contacts Discord"]
    async fn live_exchange_rejects_a_bogus_ticket_without_a_challenge() {
        let exchange = HttpTicketExchange::new().unwrap();
        let error = exchange.exchange("bogus.ticket.value").await.unwrap_err();
        println!("exchange outcome for a bogus ticket: {error:?}");
        assert!(matches!(
            error,
            RemoteAuthError::ExchangeRejected(_) | RemoteAuthError::Captcha
        ));
    }
}
