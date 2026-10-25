//! Embedded [byedpi](https://github.com/hufrea/byedpi) 0.17.3: a local proxy
//! that applies DPI desync (split, disorder, out-of-band and fake packets) to
//! the client's own connections. It needs no driver or administrator rights
//! and behaves the same on every OS; byedpi itself owns the per-OS socket
//! tricks. Origin TLS stays end to end: the engine only reorders and splits
//! the bytes the client already sends.
//!
//! byedpi keeps its state in C globals, so there is one engine per process.
//! [`enable`] starts it for the rest of the process and makes it the route of
//! every Discord HTTP and WebSocket connection ([`route_http`], [`connect`]).

use std::ffi::{CString, c_char, c_int};
use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// A built-in strategy in byedpi's option syntax.
#[derive(Debug, PartialEq, Eq)]
pub struct Preset {
    /// Saved in settings; never renamed.
    pub id: &'static str,
    pub name: &'static str,
    pub args: &'static str,
}

/// In the order [`auto_strategy`] tries them. The common desync families of
/// zapret-discord-youtube's profiles, as byedpi options; the first restored
/// Discord on the maintainer's filtered route (2026-10-25).
const PRESETS: &[Preset] = &[
    Preset {
        id: "split",
        name: "Split + TLS records",
        args: "-s1 -r1+s",
    },
    Preset {
        id: "disorder",
        name: "Disorder",
        args: "-d1",
    },
    Preset {
        id: "oob",
        name: "Out-of-band byte",
        args: "-o1 -r1+s",
    },
    Preset {
        id: "multisplit",
        name: "Split at the server name",
        args: "-s1 -d3+s -s6+s -r1+s",
    },
    // Fake packets last: byedpi has none on macOS.
    Preset {
        id: "fake",
        name: "Fake packet",
        args: "-d1 -f-1 -t5 -r1+s",
    },
];
const PORTABLE_PRESETS: usize = 4;

/// Seconds without an answer before Automatic moves a host to the next preset.
/// Direct plus every preset fits inside the 15 s WebSocket connect limit.
const AUTO_TIMEOUT_SECS: u32 = 2;

/// The presets byedpi supports on this OS.
pub fn presets() -> &'static [Preset] {
    if cfg!(target_os = "macos") {
        &PRESETS[..PORTABLE_PRESETS]
    } else {
        PRESETS
    }
}

/// Direct first, then each preset: when a host does not answer in time, resets
/// the connection or answers with a broken TLS handshake, byedpi resends the
/// same request with the next one and remembers the one that worked for that
/// address for the rest of the process.
///
/// byedpi compiles its timeout option (`-T`) only for Linux and Windows
/// (`TIMEOUT_SUPPORT` in its `params.h`), so on macOS the chain has none:
/// resets and broken handshakes still move to the next preset, while a host
/// that stays silent waits for the OS connect timeout.
pub fn auto_strategy() -> String {
    let mut strategy = if cfg!(target_os = "macos") {
        String::new()
    } else {
        format!("-T {AUTO_TIMEOUT_SECS}")
    };
    for preset in presets() {
        strategy.push_str(" -A torst,ssl_err ");
        strategy.push_str(preset.args);
    }
    strategy.trim_start().to_owned()
}

const READY_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_RESPONSE_LIMIT: usize = 1024;

static RUNNING: AtomicBool = AtomicBool::new(false);
static ENGINE: OnceLock<Engine> = OnceLock::new();

#[allow(unsafe_code)]
mod ffi {
    use std::ffi::{c_char, c_int};

    unsafe extern "C" {
        fn fastcord_byedpi_run(argc: c_int, argv: *const *mut c_char) -> c_int;
        fn fastcord_byedpi_request_stop();
    }

    /// Blocks until the engine stops. `argv` ends with a null pointer and its
    /// strings outlive the call (byedpi keeps pointers to them).
    pub(super) fn run(argv: &[*mut c_char]) -> c_int {
        // SAFETY: argv is null-terminated, its strings are owned by the caller
        // for the whole call, and only one engine runs at a time (RUNNING).
        unsafe { fastcord_byedpi_run(argv.len() as c_int - 1, argv.as_ptr()) }
    }

    /// Takes effect at the engine's next accepted connection.
    pub(super) fn request_stop() {
        // SAFETY: only sets a flag read by the engine thread.
        unsafe { fastcord_byedpi_request_stop() }
    }
}

#[derive(Debug)]
pub enum Error {
    /// Another engine is already running in this process.
    Busy,
    /// byedpi rejected the strategy arguments.
    InvalidStrategy,
    /// The engine did not start listening in time.
    NotReady,
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => f.write_str("The blocking bypass is already running."),
            Self::InvalidStrategy => f.write_str("byedpi rejected the strategy arguments."),
            Self::NotReady => f.write_str("The blocking bypass did not start."),
            Self::Io(error) => write!(f, "The blocking bypass could not start: {error}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A running byedpi listening on loopback for SOCKS5 and HTTP CONNECT.
pub struct Engine {
    addr: SocketAddr,
    thread: Option<JoinHandle<c_int>>,
}

impl Engine {
    /// `strategy` is byedpi's own command-line syntax (`-s1 -r1+s`,
    /// `-d1 -f-1 -t5`, ...), split on whitespace.
    pub fn start(strategy: &str) -> Result<Self, Error> {
        if RUNNING.swap(true, Ordering::AcqRel) {
            return Err(Error::Busy);
        }
        match Self::spawn(strategy) {
            Ok(engine) => Ok(engine),
            Err(error) => {
                RUNNING.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    fn spawn(strategy: &str) -> Result<Self, Error> {
        // ponytail: the port is free when probed and taken by byedpi a moment
        // later; a collision shows as NotReady and the user can retry.
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
            .local_addr()?
            .port();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let port = port.to_string();
        // The listener options come last so a strategy cannot move it off
        // loopback. -G also accepts HTTP CONNECT next to SOCKS5.
        let args = std::iter::once("byedpi")
            .chain(strategy.split_whitespace())
            .chain(["-G", "-i", "127.0.0.1", "-p", &port])
            .map(CString::new)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| Error::InvalidStrategy)?;
        let thread = thread::Builder::new()
            .name("byedpi".into())
            .spawn(move || {
                let mut argv: Vec<*mut c_char> =
                    args.iter().map(|arg| arg.as_ptr().cast_mut()).collect();
                argv.push(std::ptr::null_mut());
                ffi::run(&argv)
            })?;
        let mut engine = Self {
            addr,
            thread: Some(thread),
        };
        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if engine.thread.as_ref().is_some_and(JoinHandle::is_finished) {
                let _ = engine.thread.take().map(JoinHandle::join);
                return Err(Error::InvalidStrategy);
            }
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(100)).is_ok() {
                return Ok(engine);
            }
            if Instant::now() >= deadline {
                // Dropping stops and joins the thread if it did start late.
                return Err(Error::NotReady);
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            ffi::request_stop();
            // Wakes the event loop; if the engine never listened, it exits on its own.
            let _ = std::net::TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
            let _ = thread.join();
        }
        RUNNING.store(false, Ordering::Release);
    }
}

/// Starts the engine for the rest of the process and routes Discord through it.
/// Call before the first network request; there is no way back to direct.
pub fn enable(strategy: &str) -> Result<SocketAddr, Error> {
    if ENGINE.get().is_some() {
        return Err(Error::Busy);
    }
    let engine = Engine::start(strategy)?;
    let addr = engine.addr();
    ENGINE.set(engine).map_err(|_| Error::Busy)?;
    Ok(addr)
}

/// The engine address while [`enable`] is in effect.
pub fn route() -> Option<SocketAddr> {
    ENGINE.get().map(Engine::addr)
}

/// Sends the client's requests through the engine when it is enabled. HTTPS
/// uses CONNECT, so the engine resolves the name and origin TLS is unchanged.
pub fn route_http(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    match route() {
        Some(addr) => via(builder, addr),
        None => builder,
    }
}

fn via(builder: reqwest::ClientBuilder, proxy: SocketAddr) -> reqwest::ClientBuilder {
    builder
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).expect("loopback proxy URL is valid"))
}

/// Opens a TCP connection to `host:port`, through the engine when enabled.
pub async fn connect(host: &str, port: u16) -> io::Result<TcpStream> {
    match route() {
        Some(proxy) => connect_via(proxy, host, port).await,
        None => TcpStream::connect((host, port)).await,
    }
}

async fn connect_via(proxy: SocketAddr, host: &str, port: u16) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy).await?;
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    stream
        .write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await?;
    // Byte by byte so nothing after the response head (the origin's TLS) is consumed.
    let mut head = Vec::with_capacity(64);
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= CONNECT_RESPONSE_LIMIT {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized proxy response",
            ));
        }
        head.push(stream.read_u8().await?);
    }
    if head.starts_with(b"HTTP/1.1 200") || head.starts_with(b"HTTP/1.0 200") {
        Ok(stream)
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "the bypass could not reach the host",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::Mutex;

    // One engine per process: tests that start one take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the lock only serializes engines
    async fn relays_a_connection_through_connect() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let server = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = server.local_addr().unwrap().port();
        let echo = thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            let mut buf = [0; 5];
            socket.read_exact(&mut buf).unwrap();
            socket.write_all(&buf).unwrap();
        });
        // The automatic chain leaves a host that answers on the direct group.
        let engine = Engine::start(&auto_strategy()).unwrap();
        let mut stream = connect_via(engine.addr(), "127.0.0.1", port).await.unwrap();
        stream.write_all(b"hello").await.unwrap();
        let mut reply = [0; 5];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"hello");
        echo.join().unwrap();
    }

    async fn gateway_status(strategy: &str) -> Result<u16, String> {
        let engine = Engine::start(strategy).map_err(|e| e.to_string())?;
        let client = via(reqwest::Client::builder(), engine.addr())
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap();
        let response = client
            .get("https://discord.com/api/v9/gateway")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        Ok(response.status().as_u16())
    }

    /// Manual check on a filtered network (docs/TESTING.md): the anonymous
    /// Gateway URL through each preset (reported) and Automatic (asserted).
    /// `FASTCORD_BYPASS_OPTIONS` is prepended, for example `-I <physical
    /// address>` behind a VPN TUN.
    #[tokio::test]
    #[ignore = "live network"]
    #[allow(clippy::await_holding_lock)] // the lock only serializes engines
    async fn live_discord_api_through_engine() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let options = std::env::var("FASTCORD_BYPASS_OPTIONS").unwrap_or_default();
        for preset in presets() {
            let result = gateway_status(&format!("{options} {}", preset.args)).await;
            println!("{}: {result:?}", preset.name);
        }
        let started = Instant::now();
        let result = gateway_status(&format!("{options} {}", auto_strategy())).await;
        println!("Automatic: {result:?} in {:?}", started.elapsed());
        assert_eq!(result, Ok(200));
    }

    #[test]
    fn byedpi_accepts_every_preset_and_the_automatic_chain() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        for preset in presets() {
            drop(Engine::start(preset.args).unwrap());
        }
        drop(Engine::start(&auto_strategy()).unwrap());
        // byedpi has no `-T` on macOS; the chain must not offer one there.
        assert_eq!(
            auto_strategy().contains("-T "),
            !cfg!(target_os = "macos"),
            "{}",
            auto_strategy()
        );
        assert!(auto_strategy().starts_with('-'));
        // Ids are saved in settings, so they must stay distinct.
        let ids: Vec<_> = PRESETS.iter().map(|preset| preset.id).collect();
        assert!(ids.iter().enumerate().all(|(i, id)| !ids[..i].contains(id)));
    }

    #[test]
    fn restarts_and_refuses_a_second_engine() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let first = Engine::start("-d1").unwrap();
        assert!(matches!(Engine::start("-d1"), Err(Error::Busy)));
        drop(first);
        drop(Engine::start(&auto_strategy()).unwrap());
    }

    #[test]
    fn rejects_unknown_options() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        assert!(matches!(
            Engine::start("--no-such-option"),
            Err(Error::InvalidStrategy)
        ));
        assert!(matches!(
            Engine::start("-s1\0"),
            Err(Error::InvalidStrategy)
        ));
        // A rejected strategy leaves the engine free.
        drop(Engine::start(presets()[0].args).unwrap());
    }
}
