//! One local, PID-authenticated pipe. Only public TLS hellos cross this boundary.
//! The ordinary process owns all sockets, credentials and TLS verification.
#![allow(unsafe_code)]

use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use fastcord_net::tls::Strategy;
use rand_core::{OsRng, RngCore};
use tokio::sync::oneshot;
use windows::Win32::Foundation::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::IO::*;
use windows::Win32::System::Pipes::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::PCWSTR;

fn error() -> io::Error {
    io::ErrorKind::ConnectionAborted.into()
}
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}
fn handle(h: HANDLE) -> io::Result<OwnedHandle> {
    if h.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: each successful native create transfers one owned handle here.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

struct Pipe(OwnedHandle);
impl Pipe {
    fn raw(&self) -> HANDLE {
        HANDLE(self.0.as_raw_handle())
    }
    // All operations use OVERLAPPED. On timeout, cancel AND drain before the
    // event, OVERLAPPED or borrowed buffer can leave scope.
    fn operation(
        &self,
        timeout: u32,
        call: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> io::Result<usize> {
        let event =
            handle(unsafe { CreateEventW(None, true, false, None) }.map_err(io::Error::other)?)?;
        let mut ov = OVERLAPPED {
            hEvent: HANDLE(event.as_raw_handle()),
            ..Default::default()
        };
        let result = call(&mut ov);
        if let Err(e) = result {
            if e.code() == ERROR_PIPE_CONNECTED.to_hresult() {
                return Ok(0);
            }
            if e.code() != ERROR_IO_PENDING.to_hresult() {
                return Err(io::Error::other(e));
            }
            if unsafe { WaitForSingleObject(ov.hEvent, timeout) } != WAIT_OBJECT_0 {
                unsafe {
                    let _ = CancelIoEx(self.raw(), Some(&ov));
                }
                let mut ignored = 0;
                unsafe {
                    let _ = GetOverlappedResult(self.raw(), &ov, &mut ignored, true);
                }
                return Err(io::ErrorKind::TimedOut.into());
            }
        }
        let mut length = 0;
        unsafe { GetOverlappedResult(self.raw(), &ov, &mut length, true) }
            .map_err(io::Error::other)?;
        Ok(length as usize)
    }
    fn read(&self, mut bytes: &mut [u8], timeout: u32) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = self.operation(timeout, |ov| unsafe {
                ReadFile(self.raw(), Some(bytes), None, Some(ov))
            })?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            bytes = &mut bytes[n..];
        }
        Ok(())
    }
    fn write(&self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            let n = self.operation(5000, |ov| unsafe {
                WriteFile(self.raw(), Some(bytes), None, Some(ov))
            })?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            bytes = &bytes[n..];
        }
        Ok(())
    }
}

/// Lifetime of the elevated child is the lifetime of this local pipe. No
/// periodic work, network listeners, credential access or account restoration.
pub struct PacketHelper {
    pipe: Mutex<Option<Pipe>>,
    _process: OwnedHandle,
    active: AtomicUsize,
    strategy: AtomicUsize,
}
pub struct PacketFactory(pub Arc<PacketHelper>);
impl std::fmt::Debug for PacketHelper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PacketHelper")
    }
}
impl PacketHelper {
    /// Blocking UI-worker operation. Windows presents UAC to the human.
    pub fn launch() -> io::Result<Arc<Self>> {
        let mut random = [0u8; 16];
        OsRng.try_fill_bytes(&mut random).map_err(|_| error())?;
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let name = wide(std::ffi::OsStr::new(&format!(
            r"\\.\pipe\fastcord-packet-{suffix}"
        )));
        let pipe = Pipe(handle(unsafe {
            CreateNamedPipeW(
                PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                4096,
                4096,
                0,
                None,
            )
        })?);
        let executable = wide(std::env::current_exe()?.as_os_str());
        let parameters = wide(std::ffi::OsStr::new(&format!(
            "--packet-helper {suffix} {}",
            std::process::id()
        )));
        let verb = wide(std::ffi::OsStr::new("runas"));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(executable.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        unsafe { ShellExecuteExW(&mut info) }.map_err(io::Error::other)?;
        let process = handle(info.hProcess)?;
        pipe.operation(10000, |ov| unsafe {
            ConnectNamedPipe(pipe.raw(), Some(ov))
        })?;
        let mut pid = 0;
        unsafe { GetNamedPipeClientProcessId(pipe.raw(), &mut pid) }.map_err(io::Error::other)?;
        if pid == 0 || pid != unsafe { GetProcessId(HANDLE(process.as_raw_handle())) } {
            return Err(error());
        }
        let mut ready = [0; 5];
        pipe.read(&mut ready, 10000)?;
        if ready[0] != 1 {
            return Err(io::Error::from_raw_os_error(i32::from_le_bytes(
                ready[1..].try_into().map_err(|_| error())?,
            )));
        }
        Ok(Arc::new(Self {
            pipe: Mutex::new(Some(pipe)),
            _process: process,
            active: AtomicUsize::new(0),
            strategy: AtomicUsize::new(0),
        }))
    }
    fn request(
        &self,
        socket: &TcpStream,
        original: &[u8],
        fake: &[u8],
        strategy: Strategy,
    ) -> io::Result<usize> {
        let mut locked = self.pipe.lock().unwrap_or_else(|p| p.into_inner());
        let result = (|| {
            let pipe = locked.as_ref().ok_or_else(error)?;
            let header = encode(socket.peer_addr()?, original.len(), fake.len(), strategy)?;
            pipe.write(&header)?;
            pipe.write(original)?;
            pipe.write(fake)?;
            let mut status = [0];
            pipe.read(&mut status, 5000)?;
            if status != [1] {
                let mut code = [0; 4];
                pipe.read(&mut code, 5000)?;
                return Err(io::Error::from_raw_os_error(i32::from_le_bytes(code)));
            }
            // Same non-elevated process and socket, preserving per-app VPN rules.
            let mut writer = socket;
            writer.write_all(original)?;
            let mut result = [0; 5];
            pipe.read(&mut result, 5000)?;
            if result[0] != 1 {
                return Err(io::Error::from_raw_os_error(i32::from_le_bytes(
                    result[1..].try_into().map_err(|_| error())?,
                )));
            }
            #[cfg(debug_assertions)]
            eprintln!("Packet helper applied {strategy:?}.");
            Ok(original.len())
        })();
        if result.is_err() {
            #[cfg(debug_assertions)]
            if let Err(e) = &result {
                eprintln!(
                    "Packet helper request failed: kind={:?}; OS={:?}",
                    e.kind(),
                    e.raw_os_error()
                );
            }
            locked.take();
            let _ = socket.shutdown(Shutdown::Both);
        }
        result
    }
}

struct Pending {
    rx: oneshot::Receiver<io::Result<usize>>,
    socket: Arc<TcpStream>,
    finished: bool,
}
impl std::future::Future for Pending {
    type Output = io::Result<usize>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(r) => {
                self.finished = true;
                Poll::Ready(r.unwrap_or_else(|_| Err(error())))
            }
        }
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.socket.shutdown(Shutdown::Both);
        }
    }
}
impl fastcord_net::tls::FakePrefixFactory for PacketFactory {
    fn strategy(&self) -> Strategy {
        if self.0.strategy.load(Ordering::Relaxed) == 0 {
            Strategy::PacketFake
        } else {
            Strategy::PacketOverlap
        }
    }
    fn select_strategy(&self, strategy: Strategy) -> io::Result<()> {
        let mode = match strategy {
            Strategy::PacketFake => 0,
            Strategy::PacketOverlap => 1,
            _ => return Err(error()),
        };
        self.0.strategy.store(mode, Ordering::Relaxed);
        Ok(())
    }
    fn start(
        &self,
        socket: TcpStream,
        original: Vec<u8>,
        fake: Vec<u8>,
        strategy: Strategy,
    ) -> io::Result<fastcord_net::tls::PrefixFuture> {
        validate(&original, &fake)?;
        self.0
            .active
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < 4).then_some(n + 1)
            })
            .map_err(|_| io::ErrorKind::WouldBlock)?;
        let owner = self.0.clone();
        let socket = Arc::new(socket);
        let worker_socket = socket.clone();
        let (tx, rx) = oneshot::channel();
        let spawn = std::thread::Builder::new()
            .name("fastcord-packet-rpc".into())
            .spawn(move || {
                let result = owner.request(&worker_socket, &original, &fake, strategy);
                owner.active.fetch_sub(1, Ordering::Relaxed);
                let _ = tx.send(result);
            });
        if let Err(e) = spawn {
            self.0.active.fetch_sub(1, Ordering::Relaxed);
            return Err(e);
        }
        Ok(Box::pin(Pending {
            rx,
            socket,
            finished: false,
        }))
    }
}

fn encode(
    peer: SocketAddr,
    original: usize,
    fake: usize,
    strategy: Strategy,
) -> io::Result<[u8; 27]> {
    if !(43..=1024).contains(&original) || fake != original || peer.port() != 443 {
        return Err(error());
    }
    let mut h = [0; 27];
    h[..4].copy_from_slice(b"FCP2");
    match peer.ip() {
        IpAddr::V4(ip) => {
            h[4] = 4;
            h[5..9].copy_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            h[4] = 6;
            h[5..21].copy_from_slice(&ip.octets());
        }
    }
    match strategy {
        Strategy::PacketFake => {}
        Strategy::PacketOverlap => h[4] |= 128,
        _ => return Err(error()),
    }
    h[21..23].copy_from_slice(&peer.port().to_be_bytes());
    h[23..25].copy_from_slice(&(original as u16).to_be_bytes());
    h[25..27].copy_from_slice(&(fake as u16).to_be_bytes());
    Ok(h)
}
fn decode(h: &[u8; 27]) -> io::Result<(SocketAddr, usize, usize, Strategy)> {
    if &h[..4] != b"FCP2" {
        return Err(error());
    }
    let strategy = if h[4] & 128 == 0 {
        Strategy::PacketFake
    } else {
        Strategy::PacketOverlap
    };
    let ip = match h[4] & 127 {
        4 if h[9..21] == [0; 12] => IpAddr::V4(Ipv4Addr::from(
            <[u8; 4]>::try_from(&h[5..9]).map_err(|_| error())?,
        )),
        6 => IpAddr::V6(Ipv6Addr::from(
            <[u8; 16]>::try_from(&h[5..21]).map_err(|_| error())?,
        )),
        _ => return Err(error()),
    };
    let peer = SocketAddr::new(ip, u16::from_be_bytes([h[21], h[22]]));
    // A helper cannot be used for local/private endpoint injection.
    let public = fastcord_net::is_public_destination(ip);
    let a = usize::from(u16::from_be_bytes([h[23], h[24]]));
    let b = usize::from(u16::from_be_bytes([h[25], h[26]]));
    if !public || encode(peer, a, b, strategy)? != *h {
        return Err(error());
    }
    Ok((peer, a, b, strategy))
}
fn validate(original: &[u8], fake: &[u8]) -> io::Result<()> {
    if !(43..=1024).contains(&original.len())
        || original.len() != fake.len()
        || original[11..43] != fake[11..43]
    {
        return Err(error());
    }
    for hello in [original, fake] {
        match fastcord_net::tls::inspect_client_hello(hello) {
            Ok(fastcord_net::tls::Inspection::ClientHello(info))
                if info.prefix_end == hello.len()
                    && usize::from(u16::from_be_bytes([hello[3], hello[4]])) + 5 == hello.len() => {
            }
            _ => return Err(error()),
        }
    }
    Ok(())
}

/// Must run before normal application setup, even on malformed arguments.
pub fn entry(suffix: &str, parent: u32) -> io::Result<()> {
    let prefix = format!("{parent}-");
    if parent == 0
        || !suffix.starts_with(&prefix)
        || suffix.len() != prefix.len() + 32
        || !suffix[prefix.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(error());
    }
    let name = wide(std::ffi::OsStr::new(&format!(
        r"\\.\pipe\fastcord-packet-{suffix}"
    )));
    let pipe = Pipe(handle(
        unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }
        .map_err(io::Error::other)?,
    )?);
    let mut actual = 0;
    unsafe { GetNamedPipeServerProcessId(pipe.raw(), &mut actual) }.map_err(io::Error::other)?;
    if actual != parent {
        return Err(error());
    }
    // Kernel PID plus executable path, never a caller-supplied program path.
    let process = handle(
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, parent) }
            .map_err(io::Error::other)?,
    )?;
    let mut path = vec![0u16; 32768];
    let mut length = path.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            HANDLE(process.as_raw_handle()),
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path.as_mut_ptr()),
            &mut length,
        )
    }
    .map_err(io::Error::other)?;
    if std::ffi::OsString::from_wide(&path[..length as usize]) != std::env::current_exe()? {
        return Err(error());
    }
    drop(path);
    let initialized = super::packet::initialize();
    pipe.write(&status(&initialized))?;
    initialized?;
    loop {
        let mut header = [0; 27];
        pipe.read(&mut header, INFINITE)?;
        let (peer, a, b, strategy) = decode(&header)?;
        let mut original = vec![0; a];
        let mut fake = vec![0; b];
        pipe.read(&mut original, 5000)?;
        pipe.read(&mut fake, 5000)?;
        validate(&original, &fake)?;
        let result = super::packet::serve(peer, &original, &fake, strategy, || pipe.write(&[1]));
        pipe.write(&status(&result))?;
        result?;
    }
}
fn status<T>(result: &io::Result<T>) -> [u8; 5] {
    let mut bytes = [0; 5];
    bytes[0] = u8::from(result.is_ok());
    let code = result
        .as_ref()
        .err()
        .and_then(io::Error::raw_os_error)
        .unwrap_or(-1);
    bytes[1..].copy_from_slice(&code.to_le_bytes());
    bytes
}
use std::os::windows::ffi::OsStringExt;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_refuses_oversize_private_and_wrong_port() {
        let public: SocketAddr = "162.159.128.233:443".parse().unwrap();
        for n in [43, 1024] {
            for strategy in [Strategy::PacketFake, Strategy::PacketOverlap] {
                let h = encode(public, n, n, strategy).unwrap();
                assert_eq!(decode(&h).unwrap(), (public, n, n, strategy));
            }
        }
        assert!(encode(public, 1025, 1025, Strategy::PacketFake).is_err());
        assert!(encode(public, 100, 99, Strategy::PacketFake).is_err());
        assert!(
            encode(
                "162.159.128.233:80".parse().unwrap(),
                100,
                100,
                Strategy::PacketFake
            )
            .is_err()
        );
        for ip in [
            "127.0.0.1:443",
            "10.1.2.3:443",
            "[::1]:443",
            "[::ffff:127.0.0.1]:443",
        ] {
            assert!(
                decode(&encode(ip.parse().unwrap(), 100, 100, Strategy::PacketFake).unwrap())
                    .is_err()
            );
        }
    }
}
