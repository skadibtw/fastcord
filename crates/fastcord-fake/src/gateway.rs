//! The Gateway: JSON over a persistent zlib stream, like Discord's
//! `compress=zlib-stream`. HELLO, heartbeats, IDENTIFY to READY, RESUME,
//! member-list requests, and the dispatches REST calls cause.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::{Compress, Compression, FlushCompress};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

use crate::Shared;
use crate::log::Entry;
use crate::rest::Event;
use crate::world::text;

const HEARTBEAT_MS: u64 = 41_250;
const CLOSE_AUTH_FAILED: u16 = 4004;

/// The server half of a zlib-stream: one compressor for the whole connection,
/// every payload ending in a sync flush.
struct Packer(Option<Compress>);

impl Packer {
    fn pack(&mut self, json: &str) -> Message {
        let Some(compress) = &mut self.0 else {
            return Message::text(json.to_owned());
        };
        let input = json.as_bytes();
        let mut out = Vec::with_capacity(input.len() + input.len() / 8 + 256);
        let before = compress.total_in();
        // Sync flush into a buffer sized above deflate's worst case.
        let _ = compress.compress_vec(input, &mut out, FlushCompress::Sync);
        debug_assert_eq!((compress.total_in() - before) as usize, input.len());
        Message::binary(out)
    }
}

type Socket = tokio_tungstenite::WebSocketStream<TcpStream>;

struct Connection {
    shared: Arc<Shared>,
    socket: Socket,
    packer: Packer,
    query: String,
    session: Option<String>,
    seq: u64,
}

impl Connection {
    async fn send(&mut self, value: &Value) -> bool {
        let message = self.packer.pack(&value.to_string());
        self.socket.send(message).await.is_ok()
    }

    async fn dispatch(&mut self, name: &str, data: Value) -> bool {
        self.seq += 1;
        let frame = json!({"op": 0, "t": name, "s": self.seq, "d": data});
        self.send(&frame).await
    }

    async fn close(&mut self, code: u16, reason: &str) {
        let frame = CloseFrame {
            code: code.into(),
            reason: reason.to_owned().into(),
        };
        let _ = self.socket.send(Message::Close(Some(frame))).await;
        self.log("CLOSE", Some(code), None, 0);
    }

    fn log(&self, method: &str, status: Option<u16>, body: Option<Value>, bytes: usize) {
        self.shared.log.record(&Entry {
            kind: "ws",
            method,
            path: "/",
            query: &self.query,
            status,
            body_bytes: bytes,
            headers: None,
            body,
            note: None,
        });
    }

    /// Returns false when the connection should end.
    async fn on_text(&mut self, raw: &str) -> bool {
        let Ok(frame) = serde_json::from_str::<Value>(raw) else {
            self.log("MALFORMED", None, None, raw.len());
            return true;
        };
        let op = frame["op"].as_u64().unwrap_or(u64::MAX);
        self.log(&format!("OP{op}"), None, Some(frame.clone()), raw.len());
        let data = &frame["d"];
        match op {
            1 => self.send(&json!({"op": 11, "d": null})).await,
            2 | 6 => {
                let authentic = text(data, "token") == self.shared.world().token;
                if !authentic {
                    self.close(CLOSE_AUTH_FAILED, "Authentication failed.")
                        .await;
                    return false;
                }
                if op == 2 {
                    self.identify().await
                } else {
                    self.resume(data).await
                }
            }
            14 => {
                let guild = text(data, "guild_id").to_owned();
                self.member_lists(&guild, &data["channels"]).await
            }
            37 => {
                let Some(subscriptions) = data["subscriptions"].as_object() else {
                    return true;
                };
                for (guild, subscription) in subscriptions {
                    if !self.member_lists(guild, &subscription["channels"]).await {
                        return false;
                    }
                }
                true
            }
            // Presence (3), voice state (4) and the rest are logged only; voice
            // never connects to this fake.
            _ => true,
        }
    }

    async fn identify(&mut self) -> bool {
        let number = self
            .shared
            .session_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let session = format!("fake-session-{number}");
        let (ready, supplemental) = {
            let world = self.shared.world();
            (
                world.ready(&session, self.shared.port),
                world.ready_supplemental(),
            )
        };
        self.session = Some(session);
        self.dispatch("READY", ready).await
            && self.dispatch("READY_SUPPLEMENTAL", supplemental).await
    }

    async fn resume(&mut self, data: &Value) -> bool {
        let session = text(data, "session_id").to_owned();
        let seq = data["seq"].as_u64();
        let known = self
            .shared
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&session)
            .copied();
        // Events sent while disconnected are not kept, so only a session that
        // missed nothing can resume.
        if known.is_some() && known == seq {
            self.seq = known.unwrap_or(0);
            self.session = Some(session);
            self.dispatch("RESUMED", json!({})).await
        } else {
            self.send(&json!({"op": 9, "d": false})).await
        }
    }

    /// A `SYNC` of the first requested range of the guild's member list.
    async fn member_lists(&mut self, guild: &str, channels: &Value) -> bool {
        let range = channels
            .as_object()
            .and_then(|channels| channels.values().next())
            .and_then(|ranges| ranges.get(0))
            .and_then(|range| Some((range.get(0)?.as_u64()?, range.get(1)?.as_u64()?)));
        let Some(range) = range else {
            return true;
        };
        let sync = self.shared.world().member_list_sync(guild, range);
        match sync {
            Some(sync) => self.dispatch("GUILD_MEMBER_LIST_UPDATE", sync).await,
            None => true,
        }
    }
}

// The handshake callback's error type is tungstenite's own and large.
#[allow(clippy::result_large_err)]
pub(crate) async fn serve(shared: Arc<Shared>, stream: TcpStream) {
    let upgrade: Arc<Mutex<(String, BTreeMap<String, String>)>> = Arc::default();
    let capture = Arc::clone(&upgrade);
    let callback = move |req: &Request, response: Response| -> Result<Response, ErrorResponse> {
        let headers = req
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect();
        *capture.lock().unwrap_or_else(|p| p.into_inner()) =
            (req.uri().query().unwrap_or("").to_owned(), headers);
        Ok(response)
    };
    let Ok(socket) = accept_hdr_async(stream, callback).await else {
        return;
    };
    let (query, headers) = upgrade.lock().unwrap_or_else(|p| p.into_inner()).clone();
    shared.log.record(&Entry {
        kind: "ws",
        method: "CONNECT",
        path: "/",
        query: &query,
        status: Some(101),
        body_bytes: 0,
        headers: Some(&headers),
        body: None,
        note: None,
    });
    let compressed = query.split('&').any(|pair| pair == "compress=zlib-stream");
    let mut events = shared.events.subscribe();
    let mut connection = Connection {
        shared: Arc::clone(&shared),
        socket,
        packer: Packer(compressed.then(|| Compress::new(Compression::default(), true))),
        query,
        session: None,
        seq: 0,
    };
    if !connection
        .send(&json!({"op": 10, "d": {"heartbeat_interval": HEARTBEAT_MS}}))
        .await
    {
        return;
    }
    let close_after = shared.switches.close_gateway_every;
    let close_timer = tokio::time::sleep(close_after.unwrap_or(Duration::MAX));
    tokio::pin!(close_timer);
    loop {
        tokio::select! {
            () = &mut close_timer, if close_after.is_some() => {
                // Resumable: the session survives, the client must come back.
                connection.close(4000, "fake: scheduled close").await;
                break;
            }
            message = connection.socket.next() => match message {
                Some(Ok(Message::Text(raw))) => {
                    if !connection.on_text(raw.as_str()).await {
                        break;
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
            event = events.recv(), if connection.session.is_some() => match event {
                Ok(Event { name, data }) => {
                    if !connection.dispatch(name, data).await {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
        }
    }
    if let Some(session) = connection.session.take() {
        shared
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(session, connection.seq);
    }
}

const CHATTER: &[&str] = &[
    "Did you see the fake release notes?",
    "Typing a quick fake hello.",
    "Anyone around for a fake review?",
    "This message was written by the fake server.",
    "Live updates look good from here.",
];

/// Every `every`, the fixed friend types and then writes into the guild
/// channel and the DM, to exercise live updates.
pub(crate) async fn chatter(shared: Arc<Shared>, every: Duration) {
    let mut round = 0usize;
    loop {
        tokio::time::sleep(every).await;
        let (friend, targets) = {
            let world = shared.world();
            (
                world.chatter.friend.clone(),
                [
                    world.chatter.guild_channel.clone(),
                    world.chatter.dm_channel.clone(),
                ],
            )
        };
        for channel in targets {
            let guild = shared.world().guild_of_channel(&channel);
            let mut typing = json!({
                "channel_id": channel, "user_id": friend, "timestamp": crate::log::now_ms() / 1000,
            });
            if let Some(guild) = guild {
                typing["guild_id"] = guild.into();
            }
            shared.emit(vec![Event {
                name: "TYPING_START",
                data: typing,
            }]);
            tokio::time::sleep(Duration::from_millis(1500)).await;
            round += 1;
            let content = format!("{} (#{round})", CHATTER[round % CHATTER.len()]);
            let event = {
                let mut world = shared.world();
                let message = world.post(&channel, &friend, &content, None);
                Event {
                    name: "MESSAGE_CREATE",
                    data: world.with_guild(&message),
                }
            };
            shared.emit(vec![event]);
        }
    }
}
