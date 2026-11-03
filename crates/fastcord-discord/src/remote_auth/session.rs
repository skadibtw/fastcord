//! The remote-auth conversation as an explicit state machine over one
//! WebSocket. Everything secret (private key, ticket, token) stays inside this
//! function's stack frame and is zeroized or dropped when it returns.

use std::pin::Pin;
use std::time::Duration;

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use zeroize::Zeroizing;

use super::crypto::AttemptKey;
use super::exchange::{ExchangeStep, TicketExchange};
use super::protocol::{Incoming, Outgoing, RemoteUser};
use super::{QrLink, RemoteAuthError, RemoteAuthEvent};
use crate::{CaptchaChallenge, CaptchaSolution, UserToken};

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Server-provided intervals are clamped so a hostile or broken value can
/// neither spin the heartbeat nor keep an attempt alive indefinitely.
const HEARTBEAT_RANGE_MS: (u64, u64) = (1_000, 120_000);
const SESSION_RANGE_MS: (u64, u64) = (5_000, 15 * 60 * 1_000);
const MAX_TOKEN_BYTES: usize = 4096;
const CLOSE_TIMEOUT_CODE: u16 = 4003;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Nonce,
    Fingerprint,
    Scan,
    Confirm,
    Exchanging,
}

enum Flow {
    Continue,
    Ticket(Zeroizing<String>),
}

#[cfg(test)]
pub(crate) async fn drive<S, E>(
    socket: S,
    key: &AttemptKey,
    exchange: &E,
    events: &mpsc::Sender<RemoteAuthEvent>,
) -> Result<UserToken, RemoteAuthError>
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
    E: TicketExchange,
{
    drive_with_captcha(socket, key, exchange, events, &|_| async {
        Err(RemoteAuthError::Captcha)
    })
    .await
}

pub(crate) async fn drive_with_captcha<S, E, F, Fut>(
    mut socket: S,
    key: &AttemptKey,
    exchange: &E,
    events: &mpsc::Sender<RemoteAuthEvent>,
    captcha: &F,
) -> Result<UserToken, RemoteAuthError>
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
    E: TicketExchange,
    F: Fn(CaptchaChallenge) -> Fut + Send + Sync,
    Fut: Future<Output = Result<CaptchaSolution, RemoteAuthError>> + Send,
{
    let (heartbeat_every, session_for) = match timeout(HELLO_TIMEOUT, read_incoming(&mut socket))
        .await
        .map_err(|_| RemoteAuthError::Connect)??
    {
        Incoming::Hello {
            heartbeat_interval,
            timeout_ms,
        } => (
            Duration::from_millis(
                heartbeat_interval.clamp(HEARTBEAT_RANGE_MS.0, HEARTBEAT_RANGE_MS.1),
            ),
            Duration::from_millis(timeout_ms.clamp(SESSION_RANGE_MS.0, SESSION_RANGE_MS.1)),
        ),
        _ => return Err(RemoteAuthError::Protocol),
    };
    let deadline = Instant::now() + session_for;
    send(
        &mut socket,
        &Outgoing::Init {
            encoded_public_key: &key.encoded_public_key(),
        },
    )
    .await?;

    let mut stage = Stage::Nonce;
    let mut heartbeat = interval_at(Instant::now() + heartbeat_every, heartbeat_every);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut awaiting_ack = false;
    let mut connected = true;
    type Completion<'a> =
        Pin<Box<dyn Future<Output = Result<UserToken, RemoteAuthError>> + Send + 'a>>;
    let mut completion: Option<Completion<'_>> = None;
    let result = loop {
        tokio::select! {
            biased;
            () = events.closed() => break Err(RemoteAuthError::Cancelled),
            () = sleep_until(deadline) => break Err(RemoteAuthError::Expired),
            result = async {
                match completion.as_mut() {
                    Some(future) => future.await,
                    None => std::future::pending().await,
                }
            } => break result,
            incoming = read_incoming(&mut socket), if connected => {
                let incoming = match incoming {
                    Ok(incoming) => incoming,
                    // The final ticket no longer needs the QR socket. Some
                    // servers close it after phone confirmation; a human may
                    // still finish the one challenge before this deadline.
                    Err(RemoteAuthError::Closed(None | Some(1000))) if completion.is_some() => {
                        connected = false;
                        continue;
                    }
                    Err(error) => break Err(error),
                };
                if matches!(incoming, Incoming::HeartbeatAck) {
                    awaiting_ack = false;
                    continue;
                }
                match handle(incoming, &mut stage, key, events, &mut socket, deadline).await {
                    Ok(Flow::Continue) => {}
                    Ok(Flow::Ticket(ticket)) => {
                        stage = Stage::Exchanging;
                        completion = Some(Box::pin(finish_exchange(ticket, key, exchange, events, captcha)));
                    }
                    Err(error) => break Err(error),
                }
            }
            _ = heartbeat.tick(), if connected => {
                if awaiting_ack {
                    break Err(RemoteAuthError::HeartbeatLost);
                }
                if let Err(error) = send(&mut socket, &Outgoing::Heartbeat).await {
                    break Err(error);
                }
                awaiting_ack = true;
            }
        }
    };
    // Best effort: tell the gateway we are leaving; never wait long for it.
    let _ = timeout(Duration::from_secs(2), SinkExt::close(&mut socket)).await;
    result
}

async fn handle<S>(
    incoming: Incoming,
    stage: &mut Stage,
    key: &AttemptKey,
    events: &mpsc::Sender<RemoteAuthEvent>,
    socket: &mut S,
    deadline: Instant,
) -> Result<Flow, RemoteAuthError>
where
    S: Stream<Item = Result<Message, WsError>> + Sink<Message, Error = WsError> + Unpin,
{
    match (*stage, incoming) {
        (Stage::Nonce, Incoming::NonceProof { encrypted_nonce }) => {
            let proof = key.nonce_proof(&encrypted_nonce)?;
            send(socket, &Outgoing::NonceProof { nonce: &proof }).await?;
            *stage = Stage::Fingerprint;
        }
        (Stage::Fingerprint, Incoming::PendingRemoteInit { fingerprint }) => {
            // The QR code is built from our own fingerprint, and only after the
            // gateway proved it saw exactly our public key.
            if fingerprint != key.fingerprint() {
                return Err(RemoteAuthError::FingerprintMismatch);
            }
            let link = QrLink::new(
                &fingerprint,
                deadline.saturating_duration_since(Instant::now()),
            );
            emit(events, RemoteAuthEvent::Qr(link)).await?;
            *stage = Stage::Scan;
        }
        (
            Stage::Scan,
            Incoming::PendingTicket {
                encrypted_user_payload,
            },
        ) => {
            let payload = key.decrypt(&encrypted_user_payload)?;
            let user = RemoteUser::parse(&payload)?;
            emit(events, RemoteAuthEvent::PendingUser(user)).await?;
            *stage = Stage::Confirm;
        }
        (Stage::Scan | Stage::Confirm, Incoming::PendingLogin { ticket }) => {
            return Ok(Flow::Ticket(ticket));
        }
        (Stage::Scan | Stage::Confirm | Stage::Exchanging, Incoming::Cancel) => {
            return Err(RemoteAuthError::CancelledOnPhone);
        }
        (_, Incoming::Unknown) => {}
        // Hello after the first, acks handled by the caller, or any known op out
        // of sequence: the gateway is not speaking the protocol we implement.
        _ => return Err(RemoteAuthError::Protocol),
    }
    Ok(Flow::Continue)
}

async fn finish_exchange<E, F, Fut>(
    ticket: Zeroizing<String>,
    key: &AttemptKey,
    exchange: &E,
    events: &mpsc::Sender<RemoteAuthEvent>,
    captcha: &F,
) -> Result<UserToken, RemoteAuthError>
where
    E: TicketExchange,
    F: Fn(CaptchaChallenge) -> Fut + Send + Sync,
    Fut: Future<Output = Result<CaptchaSolution, RemoteAuthError>> + Send,
{
    let encrypted = match exchange.exchange(&ticket, None).await? {
        ExchangeStep::EncryptedToken(token) => token,
        ExchangeStep::Captcha(challenge) => {
            let rqtoken = challenge.rqtoken.clone();
            let session_id = challenge.session_id.clone();
            emit(events, RemoteAuthEvent::Captcha).await?;
            let solution = captcha(challenge).await?;
            if solution.key.is_empty()
                || solution.key.len() > 8192
                || !solution.key.bytes().all(|b| b.is_ascii_graphic())
                || solution.rqtoken != rqtoken
                || solution.session_id != session_id
            {
                return Err(RemoteAuthError::Protocol);
            }
            // Only an explicit challenge and a human answer permit one more
            // exchange. Ambiguous failures and repeated challenges stop here.
            match exchange.exchange(&ticket, Some(&solution)).await? {
                ExchangeStep::EncryptedToken(token) => token,
                ExchangeStep::Captcha(_) => return Err(RemoteAuthError::CaptchaRejected),
            }
        }
    };
    decrypt_token(key, &encrypted)
}

fn decrypt_token(key: &AttemptKey, encrypted_token: &str) -> Result<UserToken, RemoteAuthError> {
    let mut plaintext = key.decrypt(encrypted_token)?;
    let valid = !plaintext.is_empty()
        && plaintext.len() <= MAX_TOKEN_BYTES
        && plaintext.iter().all(u8::is_ascii_graphic);
    if !valid {
        return Err(RemoteAuthError::Crypto);
    }
    // The decrypted buffer moves into the wrapper without being copied; the
    // wrapper zeroizes it on drop.
    let bytes = std::mem::take(&mut *plaintext);
    String::from_utf8(bytes)
        .map(UserToken::new)
        .map_err(|error| {
            drop(Zeroizing::new(error.into_bytes()));
            RemoteAuthError::Crypto
        })
}

async fn emit(
    events: &mpsc::Sender<RemoteAuthEvent>,
    event: RemoteAuthEvent,
) -> Result<(), RemoteAuthError> {
    events
        .send(event)
        .await
        .map_err(|_| RemoteAuthError::Cancelled)
}

async fn send<S>(socket: &mut S, message: &Outgoing<'_>) -> Result<(), RemoteAuthError>
where
    S: Sink<Message, Error = WsError> + Unpin,
{
    let text = message.to_text()?;
    timeout(SEND_TIMEOUT, socket.send(Message::text(text.as_str())))
        .await
        .map_err(|_| RemoteAuthError::Closed(None))?
        .map_err(|_| RemoteAuthError::Closed(None))
}

/// Next protocol message; control frames are consumed, a close frame becomes a
/// categorical error, and binary frames are a protocol violation.
async fn read_incoming<S>(socket: &mut S) -> Result<Incoming, RemoteAuthError>
where
    S: Stream<Item = Result<Message, WsError>> + Unpin,
{
    loop {
        match socket.next().await {
            Some(Ok(Message::Text(text))) => return Incoming::parse(text.as_str()),
            Some(Ok(Message::Ping(_) | Message::Pong(_) | Message::Frame(_))) => {}
            Some(Ok(Message::Binary(_))) => return Err(RemoteAuthError::Protocol),
            Some(Ok(Message::Close(frame))) => {
                return Err(match frame.map(|frame| u16::from(frame.code)) {
                    Some(CLOSE_TIMEOUT_CODE) => RemoteAuthError::Expired,
                    code => RemoteAuthError::Closed(code),
                });
            }
            Some(Err(_)) | None => return Err(RemoteAuthError::Closed(None)),
        }
    }
}

#[cfg(test)]
mod tests;
