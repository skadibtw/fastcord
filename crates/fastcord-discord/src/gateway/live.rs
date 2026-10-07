//! Manual checks against Discord's real Gateway (open risk U1). Never run in CI.
//!
//! - Unauthenticated, needs only network:
//!   `cargo test -p fastcord-discord live_ --locked -- --ignored --nocapture`
//!   exercises the real discovery endpoint, TLS WebSocket, real zlib-stream
//!   HELLO, the real Identify payload, and the real rejection of a bogus token.
//! - With the alt account (never a main account): set `FASTCORD_LIVE_TOKEN` in
//!   the *process environment* of that one command (never in a file) and run
//!   `cargo test -p fastcord-discord live_account --locked -- --ignored --nocapture`.
//!   It Identifies, forces a reconnect, and checks that the session Resumes
//!   without a second READY. Output contains counts and state names only.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Sink, Stream};
use tokio::sync::watch;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::{Error as WsError, Message};

use super::pacing::OsJitter;
use super::profile::resolve_build_number;
use super::transport::{DiscoverError, GatewayInfo, LiveTransport, Transport, connect_tls};
use super::url::GatewayUrl;
use super::*;
use crate::RestClient;
use crate::ws::{ConnectError, TlsSocket};

/// Real discovery and connection without any credential: the account's REST
/// client is not involved, so a bogus token reaches the Gateway itself.
struct Unauthenticated;

impl Transport for Unauthenticated {
    type Socket = TlsSocket;

    async fn build_number(&self) -> BuildNumber {
        resolve_build_number().await
    }

    async fn discover(&self) -> Result<String, DiscoverError> {
        let response = reqwest::get("https://discord.com/api/v10/gateway")
            .await
            .map_err(|_| DiscoverError::Failed)?;
        let body = response.bytes().await.map_err(|_| DiscoverError::Failed)?;
        let info: GatewayInfo = serde_json::from_slice(&body).map_err(|_| DiscoverError::Failed)?;
        Ok(info.url)
    }

    async fn connect(&self, url: &GatewayUrl) -> Result<TlsSocket, ConnectError> {
        connect_tls(url).await
    }

    async fn authentication_required(&self) {
        std::future::pending().await
    }

    fn stop_authenticated_work(&self) {}
}

/// A socket that fails like a dropped network when told to.
struct Cuttable {
    inner: TlsSocket,
    cut: Pin<Box<dyn Future<Output = ()> + Send>>,
    cut_done: bool,
}

impl Stream for Cuttable {
    type Item = Result<Message, WsError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if !this.cut_done && this.cut.as_mut().poll(cx).is_ready() {
            this.cut_done = true;
        }
        if this.cut_done {
            return Poll::Ready(Some(Err(WsError::Io(
                std::io::ErrorKind::ConnectionReset.into(),
            ))));
        }
        Pin::new(&mut this.inner).poll_next(cx)
    }
}

impl Sink<Message> for Cuttable {
    type Error = WsError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        Pin::new(&mut self.inner).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), WsError> {
        Pin::new(&mut self.inner).start_send(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), WsError>> {
        Pin::new(&mut self.inner).poll_close(cx)
    }
}

/// The real transport, plus a switch that kills the current connection.
struct Severable {
    inner: LiveTransport,
    cut: watch::Sender<u64>,
}

impl Transport for Severable {
    type Socket = Cuttable;

    async fn build_number(&self) -> BuildNumber {
        self.inner.build_number().await
    }

    async fn discover(&self) -> Result<String, DiscoverError> {
        self.inner.discover().await
    }

    async fn connect(&self, url: &GatewayUrl) -> Result<Cuttable, ConnectError> {
        let inner = self.inner.connect(url).await?;
        let mut cut = self.cut.subscribe();
        let seen = *cut.borrow();
        Ok(Cuttable {
            inner,
            cut: Box::pin(async move {
                let _ = cut.wait_for(|generation| *generation > seen).await;
            }),
            cut_done: false,
        })
    }

    async fn authentication_required(&self) {
        self.inner.authentication_required().await;
    }

    fn stop_authenticated_work(&self) {
        self.inner.stop_authenticated_work();
    }
}

async fn next_within(gateway: &mut Gateway, limit: Duration) -> GatewayEvent {
    timeout(limit, gateway.next_event())
        .await
        .expect("no event within the time limit")
        .expect("the gateway ended")
}

#[tokio::test]
#[ignore = "contacts Discord"]
async fn live_real_gateway_negotiates_zlib_stream_and_rejects_a_bogus_token() {
    let token = Arc::new(UserToken::new("bogus.invalid.token".to_owned()));
    let mut gateway = start_with(
        Arc::new(Unauthenticated),
        token,
        "en-US".to_owned(),
        OsJitter,
    );
    let mut states = Vec::new();
    loop {
        match next_within(&mut gateway, Duration::from_secs(30)).await {
            GatewayEvent::State(state) => {
                println!("state: {state:?}");
                let terminal = state.is_terminal();
                states.push(state);
                if terminal {
                    break;
                }
            }
            other => panic!("unexpected event before authentication: {other:?}"),
        }
    }
    // Hello arrived over the real compressed stream and Identify was parsed:
    // a malformed payload would have ended in close code 4002, not 4004.
    assert_eq!(
        states.last(),
        Some(&ConnectionState::AuthenticationRequired),
        "{states:?}"
    );
    assert!(states.contains(&ConnectionState::Identifying));
}

#[tokio::test]
#[ignore = "needs FASTCORD_LIVE_TOKEN (alt account) in the environment; contacts Discord"]
async fn live_account_identifies_and_resumes_after_a_forced_reconnect() {
    let token = std::env::var("FASTCORD_LIVE_TOKEN").expect("set FASTCORD_LIVE_TOKEN for this run");
    let rest = RestClient::new(UserToken::new(token.clone())).unwrap();
    let (cut, _) = watch::channel(0u64);
    let transport = Arc::new(Severable {
        inner: LiveTransport::new(rest),
        cut: cut.clone(),
    });
    let mut gateway = start_with(
        transport,
        Arc::new(UserToken::new(token)),
        "en-US".to_owned(),
        OsJitter,
    );
    let limit = Duration::from_secs(90);

    // Identify through READY (and READY_SUPPLEMENTAL, which follows).
    let mut supplemental = false;
    let mut readied = false;
    while !(supplemental && readied) {
        match next_within(&mut gateway, limit).await {
            GatewayEvent::State(ConnectionState::Ready) => readied = true,
            GatewayEvent::State(state) => {
                println!("state: {state:?}");
                assert!(!state.is_terminal(), "{state:?}");
            }
            GatewayEvent::Dispatch { sequence, event } => match event {
                Dispatch::Ready(ready) => println!(
                    "READY seq {sequence}: {} guilds, {} unavailable, {} private channels, {} users",
                    ready.guilds.len(),
                    ready.unavailable_guilds.len(),
                    ready.private_channels.len(),
                    ready.users.len()
                ),
                Dispatch::ReadySupplemental(extra) => {
                    supplemental = true;
                    println!(
                        "READY_SUPPLEMENTAL seq {sequence}: {} guilds, {} lazy private channels",
                        extra.guilds.len(),
                        extra.lazy_private_channels.len()
                    );
                }
                other => println!("dispatch seq {sequence}: {}", other.name()),
            },
        }
    }

    // Kill the connection as a network drop would, and expect a Resume.
    tokio::time::sleep(Duration::from_secs(3)).await;
    cut.send_modify(|generation| *generation += 1);
    let mut resumed = false;
    let mut resuming = false;
    while !(resumed && resuming) {
        match next_within(&mut gateway, limit).await {
            GatewayEvent::State(ConnectionState::Resuming) => resuming = true,
            GatewayEvent::State(state) => {
                println!("state: {state:?}");
                assert!(!state.is_terminal(), "{state:?}");
            }
            GatewayEvent::Dispatch { event, .. } => {
                assert!(
                    !matches!(event, Dispatch::Ready(_)),
                    "a Resume must not produce a second READY"
                );
                if matches!(event, Dispatch::Resumed) {
                    resumed = true;
                }
            }
        }
    }
    println!("resumed without a new READY");
}
