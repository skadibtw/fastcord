//! `fastcord-fake`: a local fake Discord (REST plus zlib-stream Gateway) for
//! clicking through the real app without a Discord account. Test-only: it
//! binds 127.0.0.1, invents its own world, and is never packaged.

pub mod auth;
mod gateway;
pub mod http;
pub mod log;
pub mod ratelimit;
pub mod redact;
pub mod rest;
pub mod world;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use http::{ParseError, Request, Response};
use log::{Entry, Log};
use rest::{Ctx, Event};
use world::World;

/// The world compiled in, so the binary runs from any directory.
pub const DEFAULT_WORLD: &str = include_str!("../../../fixtures/fake/world.json");

pub struct Config {
    /// 0 picks a free port.
    pub port: u16,
    /// A world file; the bundled one when `None`.
    pub world: Option<PathBuf>,
    /// The JSON-lines request log; none when `None`.
    pub log: Option<PathBuf>,
    /// Print each request to stdout.
    pub echo: bool,
    /// A friend writes every this often.
    pub chatter: Option<Duration>,
    /// Test switches for the client's traffic limits; all off by default.
    pub switches: Switches,
}

/// Faults the fake can be told to produce, so the client's limits can be
/// tested end to end.
#[derive(Clone, Copy, Debug, Default)]
pub struct Switches {
    /// Every REST request is answered 429 with `retry_after: 0`.
    pub zero_retry_429: bool,
    /// Each Gateway connection is closed (code 4000, resumable) this long
    /// after it opens.
    pub close_gateway_every: Option<Duration>,
    /// `GET /gateway` answers 403, as a Cloudflare block or a ban would.
    pub forbid_gateway_discovery: bool,
}

pub(crate) struct Shared {
    world: Mutex<World>,
    events: broadcast::Sender<Event>,
    log: Log,
    limiter: Mutex<ratelimit::RateLimiter>,
    port: u16,
    rate_limited: AtomicU64,
    sessions: Mutex<std::collections::HashMap<String, u64>>,
    session_counter: AtomicU64,
    switches: Switches,
}

impl Shared {
    pub(crate) fn emit(&self, events: Vec<Event>) {
        for event in events {
            // No listener is fine: nobody is connected.
            let _ = self.events.send(event);
        }
    }

    pub(crate) fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn answer(&self, req: &Request) -> (Response, Option<&'static str>) {
        let rate_key = ratelimit::route_key(&req.method, &req.path);
        if req.path.starts_with(rest::PREFIX) && self.switches.zero_retry_429 {
            self.rate_limited.fetch_add(1, Ordering::Relaxed);
            let mut response = Response::json(
                429,
                &json!({"message": "You are being rate limited.", "retry_after": 0, "global": false}),
            );
            response.headers.push(("Retry-After", "0".to_owned()));
            return (response, Some("RATE_LIMITED"));
        }
        if self.switches.forbid_gateway_discovery && req.path == format!("{}/gateway", rest::PREFIX)
        {
            let response = Response::json(403, &json!({"message": "fake: forbidden", "code": 0}));
            return (response, None);
        }
        // Login 429s are a scenario (`limited@`), not a count.
        if req.path.starts_with(rest::PREFIX) && !req.path.contains("/auth/") {
            let limited = self
                .limiter
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .check(&rate_key, Instant::now());
            if let Some(wait) = limited {
                self.rate_limited.fetch_add(1, Ordering::Relaxed);
                return (too_many(&rate_key, wait), Some("RATE_LIMITED"));
            }
        }
        let outcome = {
            let mut world = self.world();
            rest::handle(&mut world, req, &Ctx { port: self.port })
        };
        self.emit(outcome.events);
        let note = outcome.unimplemented.then_some("UNIMPLEMENTED");
        (outcome.response, note)
    }

    fn log_http(&self, req: &Request, response: &Response, note: Option<&'static str>) {
        self.log.record(&Entry {
            kind: "http",
            method: &req.method,
            path: &req.path,
            query: &req.query,
            status: Some(response.status),
            body_bytes: req.body.len(),
            headers: Some(&req.headers),
            body: body_for_log(req),
            note,
        });
    }
}

fn body_for_log(req: &Request) -> Option<Value> {
    if req.body.is_empty() {
        return None;
    }
    Some(
        req.json()
            .unwrap_or_else(|| json!({"_not_json_bytes": req.body.len()})),
    )
}

fn too_many(key: &str, wait: Duration) -> Response {
    let seconds = wait.as_secs_f64();
    let mut response = Response::json(
        429,
        &json!({"message": "You are being rate limited.", "retry_after": seconds, "global": false}),
    );
    let bucket: u64 = key.bytes().fold(14_695_981_039_346_656_037, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(1_099_511_628_211)
    });
    response.headers.extend([
        ("Retry-After", format!("{}", wait.as_secs() + 1)),
        ("X-RateLimit-Limit", ratelimit::LIMIT.to_string()),
        ("X-RateLimit-Remaining", "0".to_owned()),
        ("X-RateLimit-Reset-After", format!("{seconds:.3}")),
        (
            "X-RateLimit-Reset",
            format!("{:.3}", (log::now_ms() as f64 + seconds * 1000.0) / 1000.0),
        ),
        ("X-RateLimit-Bucket", format!("fake-{bucket:016x}")),
        ("X-RateLimit-Scope", "user".to_owned()),
    ]);
    response
}

async fn serve_http(shared: Arc<Shared>, mut stream: TcpStream) {
    let mut buf = Vec::new();
    loop {
        match http::read_request(&mut stream, &mut buf).await {
            Ok(Some(Ok(request))) => {
                let (response, note) = shared.answer(&request);
                shared.log_http(&request, &response, note);
                let close = request.wants_close();
                if http::write_response(&mut stream, &response, close)
                    .await
                    .is_err()
                    || close
                {
                    return;
                }
            }
            Ok(Some(Err(error))) => {
                let (status, message) = match error {
                    ParseError::Malformed => (400, "fake: malformed request"),
                    ParseError::TooLarge => (413, "fake: request too large"),
                    ParseError::Chunked => (411, "fake: chunked bodies are not supported"),
                };
                let response = Response::json(status, &json!({"message": message, "code": 0}));
                let _ = http::write_response(&mut stream, &response, true).await;
                let _ = stream.shutdown().await;
                return;
            }
            Ok(None) | Err(_) => return,
        }
    }
}

/// Whether the first bytes of the connection are a WebSocket upgrade.
async fn is_upgrade(stream: &TcpStream) -> bool {
    let mut head = [0u8; 4096];
    for _ in 0..50 {
        let Ok(n) = stream.peek(&mut head).await else {
            return false;
        };
        let text = String::from_utf8_lossy(&head[..n]).to_ascii_lowercase();
        if text.contains("\r\n\r\n") || n == head.len() {
            return text.contains("upgrade: websocket");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    false
}

/// A running fake. Dropping it stops serving.
pub struct Handle {
    addr: SocketAddr,
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl Handle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// How many requests have been answered 429 so far.
    pub fn rate_limited(&self) -> u64 {
        self.shared.rate_limited.load(Ordering::Relaxed)
    }

    /// Automatic acknowledgements that did not carry the token last issued
    /// for their channel.
    pub fn ack_token_mismatches(&self) -> u64 {
        self.shared.world().ack_token_mismatches
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

pub async fn start(config: Config) -> Result<Handle, String> {
    let text = match &config.world {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read world {}: {e}", path.display()))?,
        None => DEFAULT_WORLD.to_owned(),
    };
    let world = World::load(&text)?;
    let listener = TcpListener::bind(("127.0.0.1", config.port))
        .await
        .map_err(|e| format!("cannot bind 127.0.0.1:{}: {e}", config.port))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let log = Log::open(config.log.as_deref(), config.echo)
        .map_err(|e| format!("cannot open the request log: {e}"))?;
    let shared = Arc::new(Shared {
        world: Mutex::new(world),
        events: broadcast::channel(256).0,
        log,
        limiter: Mutex::default(),
        port: addr.port(),
        rate_limited: AtomicU64::new(0),
        sessions: Mutex::default(),
        session_counter: AtomicU64::new(0),
        switches: config.switches,
    });
    let mut tasks = Vec::new();
    let accept = Arc::clone(&shared);
    tasks.push(tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let _ = stream.set_nodelay(true);
            let shared = Arc::clone(&accept);
            tokio::spawn(async move {
                if is_upgrade(&stream).await {
                    gateway::serve(shared, stream).await;
                } else {
                    serve_http(shared, stream).await;
                }
            });
        }
    }));
    if let Some(every) = config.chatter {
        tasks.push(tokio::spawn(gateway::chatter(Arc::clone(&shared), every)));
    }
    Ok(Handle {
        addr,
        shared,
        tasks,
    })
}
