//! Linux packet-helper admission/lifetime core. No interception, elevation or
//! firewall changes occur here. The shipped helper remains Unsupported.
//!
//! All calls belong on a dedicated worker. A consumer must remove its owned
//! interception rule before releasing its Lease; cancellation invalidates the
//! lease immediately but its socket stays pinned until the consumer drops it.
#![allow(unsafe_code)]

use std::io;
use std::mem::{size_of, zeroed};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

pub const MAX_LEASES: usize = 4;
pub const MAX_FRAME: usize = 106;
pub const LEASE_TIME: Duration = Duration::from_secs(4);
const READY: &[u8] = b"FCL1";
const REGISTER: u8 = 1;
const CANCEL: u8 = 2;
const STOP: u8 = 3;

fn invalid() -> io::Error {
    io::ErrorKind::InvalidData.into()
}
fn denied() -> io::Error {
    io::ErrorKind::PermissionDenied.into()
}

/// Expected client identity comes from the launcher, never from the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Peer {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}
impl Peer {
    pub fn current() -> Self {
        // SAFETY: these calls have no pointer arguments or side effects.
        unsafe {
            Self {
                pid: libc::getpid() as u32,
                uid: libc::geteuid(),
                gid: libc::getegid(),
            }
        }
    }
    fn matches(self, credentials: libc::ucred) -> bool {
        credentials.pid > 0
            && self.pid == credentials.pid as u32
            && self.uid == credentials.uid
            && self.gid == credentials.gid
    }
}

fn option<T: Copy>(fd: RawFd, name: libc::c_int) -> io::Result<T> {
    // SAFETY: getsockopt writes only into the exclusively borrowed output and
    // its length. All private callers choose the kernel option's exact type.
    let mut value: T = unsafe { zeroed() };
    let mut length = size_of::<T>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            name,
            (&mut value as *mut T).cast(),
            &mut length,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    if length as usize != size_of::<T>() {
        return Err(invalid());
    }
    Ok(value)
}

struct Flow {
    socket: UdpSocket,
    id: u64,
    cookie: u64,
    local: SocketAddr,
    peer: SocketAddr,
    request: [u8; 74],
    until: Instant,
    active: AtomicBool,
}

/// Keeping this handle pins the socket even after cancellation/peer exit.
/// Check is_active immediately before use, remove the rule, then drop it.
#[derive(Clone)]
pub struct Lease(Arc<Flow>);
impl std::fmt::Debug for Lease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("id", &self.id())
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}
impl Lease {
    pub fn id(&self) -> u64 {
        self.0.id
    }
    pub fn cookie(&self) -> u64 {
        self.0.cookie
    }
    pub fn local(&self) -> SocketAddr {
        self.0.local
    }
    pub fn peer(&self) -> SocketAddr {
        self.0.peer
    }
    pub fn request(&self) -> &[u8; 74] {
        &self.0.request
    }
    pub fn is_active(&self) -> bool {
        self.0.active.load(Ordering::Acquire) && Instant::now() < self.0.until
    }
    /// Recheck mutable socket properties before installing/using a rule.
    pub fn validate(&self) -> io::Result<()> {
        if !self.is_active()
            || self.0.socket.local_addr()? != self.local()
            || option::<u64>(self.0.socket.as_raw_fd(), libc::SO_COOKIE)? != self.cookie()
        {
            return Err(denied());
        }
        exclusive(self.0.socket.as_raw_fd())?;
        match self.0.socket.peer_addr() {
            Ok(peer) if peer != self.peer() => Err(denied()),
            Err(error) if error.kind() != io::ErrorKind::NotConnected => Err(error),
            _ => Ok(()),
        }
    }
    fn retire(&self) {
        self.0.active.store(false, Ordering::Release);
    }
}

#[derive(Debug)]
pub enum Event {
    Registered(Lease),
    Retired(u64),
    Stopped,
}

/// Bounded single-client session. Linux SO_PEERPIDFD (6.5+) is required so
/// process lifetime is tied atomically to this connection, without PID reuse.
pub struct Session {
    control: Option<OwnedFd>,
    parent: OwnedFd,
    expected: Peer,
    network: u64,
    last_id: u64,
    leases: Vec<Lease>,
    retired: Vec<Weak<Flow>>,
    stopped: bool,
    allow_loopback: bool,
}
impl Session {
    pub fn new(control: OwnedFd, expected: Peer) -> io::Result<Self> {
        Self::create(control, expected, false)
    }
    fn create(control: OwnedFd, expected: Peer, allow_loopback: bool) -> io::Result<Self> {
        let fd = control.as_raw_fd();
        if option::<i32>(fd, libc::SO_DOMAIN)? != libc::AF_UNIX
            || option::<i32>(fd, libc::SO_TYPE)? != libc::SOCK_SEQPACKET
            || !expected.matches(option(fd, libc::SO_PEERCRED)?)
        {
            return Err(denied());
        }
        let raw: i32 = option(fd, libc::SO_PEERPIDFD)?;
        if raw < 0 {
            return Err(invalid());
        }
        // SAFETY: SO_PEERPIDFD returns a new owned close-on-exec descriptor.
        let parent = unsafe { OwnedFd::from_raw_fd(raw) };
        let enabled: i32 = 1;
        // SAFETY: initialized integer input remains live during setsockopt.
        if unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&enabled as *const i32).cast(),
                size_of::<i32>() as _,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // An unbound Unix IPC socket witnesses this worker's network namespace.
        // No application transport socket is created or bound outside net.
        // SAFETY: no pointer arguments; successful socket returns one owned FD.
        let witness =
            unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
        if witness < 0 {
            return Err(io::Error::last_os_error());
        }
        let witness = unsafe { OwnedFd::from_raw_fd(witness) };
        let network = option(witness.as_raw_fd(), libc::SO_NETNS_COOKIE)?;
        // Client must wait for READY before sending; credentials are enabled.
        // SAFETY: fixed live byte buffer, connected owned seqpacket socket.
        if unsafe {
            libc::send(
                fd,
                READY.as_ptr().cast(),
                READY.len(),
                libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
            )
        } != READY.len() as isize
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            control: Some(control),
            parent,
            expected,
            network,
            last_id: 0,
            leases: Vec::with_capacity(MAX_LEASES),
            retired: Vec::with_capacity(MAX_LEASES),
            stopped: false,
            allow_loopback,
        })
    }

    /// A timeout is only an idle wait limit; lease deadlines never reset.
    /// Any malformed/authentication/I/O failure retires the whole session.
    pub fn next(&mut self, timeout: Option<Duration>) -> io::Result<Event> {
        let result = self.next_inner(timeout);
        if result
            .as_ref()
            .is_err_and(|e| e.kind() != io::ErrorKind::TimedOut)
        {
            self.stop();
        }
        result
    }
    fn next_inner(&mut self, timeout: Option<Duration>) -> io::Result<Event> {
        if self.stopped {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let limit = timeout
            .map(|wait| Instant::now().checked_add(wait).ok_or_else(invalid))
            .transpose()?;
        loop {
            let now = Instant::now();
            if let Some(event) = self.expire(now) {
                return Ok(event);
            }
            if limit.is_some_and(|limit| now >= limit) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let due = self
                .leases
                .iter()
                .map(|lease| lease.0.until)
                .chain(limit)
                .min();
            let millis = due.map_or(-1, |due| {
                due.saturating_duration_since(now)
                    .as_millis()
                    .saturating_add(1)
                    .min(i32::MAX as u128) as i32
            });
            let mut descriptors = [
                libc::pollfd {
                    fd: self
                        .control
                        .as_ref()
                        .ok_or(io::ErrorKind::BrokenPipe)?
                        .as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.parent.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            // SAFETY: poll sees exactly this writable two-element array.
            let ready =
                unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, millis) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready == 0 {
                continue;
            }
            if descriptors[1].revents != 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if descriptors[0].revents & libc::POLLIN != 0 {
                return self.receive();
            }
            if descriptors[0].revents != 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
        }
    }
    fn retire(&mut self, index: usize) -> Event {
        let lease = self.leases.swap_remove(index);
        lease.retire();
        self.retired.push(Arc::downgrade(&lease.0));
        Event::Retired(lease.id())
    }
    fn expire(&mut self, now: Instant) -> Option<Event> {
        self.leases
            .iter()
            .position(|lease| now >= lease.0.until)
            .map(|index| self.retire(index))
    }
    fn receive(&mut self) -> io::Result<Event> {
        self.retired.retain(|lease| lease.strong_count() != 0);
        let (frame, fds, credentials) = receive(
            self.control
                .as_ref()
                .ok_or(io::ErrorKind::BrokenPipe)?
                .as_raw_fd(),
        )?;
        if !self.expected.matches(credentials) || frame.len() < 13 || &frame[..4] != READY {
            return Err(denied());
        }
        let id = u64::from_be_bytes(frame[5..13].try_into().map_err(|_| invalid())?);
        match frame[4] {
            REGISTER
                if frame.len() == MAX_FRAME
                    && fds.len() == 1
                    && id > self.last_id
                    && self.leases.len() + self.retired.len() < MAX_LEASES =>
            {
                let peer = decode_peer(&frame[13..32])?;
                if peer.port() == 0
                    || !(fastcord_net::is_public_destination(peer.ip())
                        || self.allow_loopback && peer.ip().is_loopback())
                {
                    return Err(denied());
                }
                let request: [u8; 74] = frame[32..].try_into().map_err(|_| invalid())?;
                if !fastcord_net::udp::is_discovery_request(&request) {
                    return Err(invalid());
                }
                let fd = fds.into_iter().next().ok_or_else(invalid)?;
                let raw = fd.as_raw_fd();
                if option::<i32>(raw, libc::SO_TYPE)? != libc::SOCK_DGRAM
                    || option::<i32>(raw, libc::SO_PROTOCOL)? != libc::IPPROTO_UDP
                    || option::<i32>(raw, libc::SO_DOMAIN)?
                        != if peer.is_ipv4() {
                            libc::AF_INET
                        } else {
                            libc::AF_INET6
                        }
                    || option::<u64>(raw, libc::SO_NETNS_COOKIE)? != self.network
                {
                    return Err(denied());
                }
                exclusive(raw)?;
                let cookie = option::<u64>(raw, libc::SO_COOKIE)?;
                // Wrap the already-created, received application socket; this
                // does not construct, connect or bind a new UDP transport.
                let socket: UdpSocket = fd.into();
                let local = socket.local_addr()?;
                if local.port() == 0 {
                    return Err(denied());
                }
                let lease = Lease(Arc::new(Flow {
                    socket,
                    id,
                    cookie,
                    local,
                    peer,
                    request,
                    until: Instant::now() + LEASE_TIME,
                    active: AtomicBool::new(true),
                }));
                lease.validate()?;
                if self.leases.iter().any(|old| old.cookie() == cookie)
                    || self
                        .retired
                        .iter()
                        .filter_map(Weak::upgrade)
                        .any(|old| old.cookie == cookie)
                {
                    return Err(denied());
                }
                self.last_id = id;
                self.leases.push(lease.clone());
                Ok(Event::Registered(lease))
            }
            CANCEL if frame.len() == 13 && fds.is_empty() && id != 0 && id <= self.last_id => {
                // Cancellation can race a deadline or an earlier cancellation.
                // A late old ID must never retire a replacement or the session.
                Ok(self
                    .leases
                    .iter()
                    .position(|lease| lease.id() == id)
                    .map_or(Event::Retired(id), |index| self.retire(index)))
            }
            STOP if frame.len() == 13 && id == 0 && fds.is_empty() => {
                self.stop();
                Ok(Event::Stopped)
            }
            _ => Err(invalid()),
        }
    }
    fn stop(&mut self) {
        self.stopped = true;
        self.control.take(); // Close queued ancillary rights and stop further admission.
        for lease in &self.leases {
            lease.retire();
        }
        self.leases.clear();
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

fn exclusive(fd: RawFd) -> io::Result<()> {
    if option::<i32>(fd, libc::SO_REUSEADDR)? != 0 || option::<i32>(fd, libc::SO_REUSEPORT)? != 0 {
        Err(denied())
    } else {
        Ok(())
    }
}

fn decode_peer(bytes: &[u8]) -> io::Result<SocketAddr> {
    let port = u16::from_be_bytes(bytes[17..19].try_into().map_err(|_| invalid())?);
    match bytes[0] {
        4 if bytes[5..17].iter().all(|byte| *byte == 0) => Ok(SocketAddr::from((
            <[u8; 4]>::try_from(&bytes[1..5]).map_err(|_| invalid())?,
            port,
        ))),
        6 => Ok(SocketAddr::from((
            <[u8; 16]>::try_from(&bytes[1..17]).map_err(|_| invalid())?,
            port,
        ))),
        _ => Err(invalid()),
    }
}

/// Decode ancillary ownership before rejecting anything, so every received FD
/// closes on every malformed/truncated/error path. Kernel closes excess rights
/// that do not fit the fixed ancillary buffer.
fn receive(fd: RawFd) -> io::Result<(Vec<u8>, Vec<OwnedFd>, libc::ucred)> {
    let mut frame = [0u8; MAX_FRAME];
    let mut ancillary = [0u64; 16];
    let mut vector = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    // SAFETY: zero is the empty msghdr representation; all writable buffers
    // and aligned ancillary storage remain valid throughout recvmsg/parsing.
    let mut message: libc::msghdr = unsafe { zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = ancillary.as_mut_ptr().cast();
    message.msg_controllen = size_of_val(&ancillary);
    let length = unsafe {
        libc::recvmsg(
            fd,
            &mut message,
            libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT,
        )
    };
    if length < 0 {
        return Err(io::Error::last_os_error());
    }
    // The fixed 128-byte ancillary buffer can hold fewer than 24 descriptors.
    let mut fds = Vec::with_capacity(24);
    let mut credentials = None;
    let mut malformed = false;
    // SAFETY: kernel-built control records, traversed only within msg_controllen.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            let head = &*header;
            let base = libc::CMSG_LEN(0) as usize;
            if head.cmsg_len < base {
                malformed = true;
                break;
            }
            let size = head.cmsg_len - base;
            if head.cmsg_level == libc::SOL_SOCKET && head.cmsg_type == libc::SCM_RIGHTS {
                if !size.is_multiple_of(size_of::<i32>()) {
                    malformed = true;
                }
                for index in 0..size / size_of::<i32>() {
                    let raw = std::ptr::read_unaligned(
                        libc::CMSG_DATA(header)
                            .add(index * size_of::<i32>())
                            .cast::<i32>(),
                    );
                    if raw >= 0 {
                        fds.push(OwnedFd::from_raw_fd(raw));
                    } else {
                        malformed = true;
                    }
                }
            } else if head.cmsg_level == libc::SOL_SOCKET
                && head.cmsg_type == libc::SCM_CREDENTIALS
                && size == size_of::<libc::ucred>()
            {
                if credentials
                    .replace(std::ptr::read_unaligned(
                        libc::CMSG_DATA(header).cast::<libc::ucred>(),
                    ))
                    .is_some()
                {
                    malformed = true;
                }
            } else {
                malformed = true;
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if length == 0 {
        return Err(io::ErrorKind::BrokenPipe.into());
    }
    if malformed || message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || fds.len() > 1 {
        return Err(invalid());
    }
    Ok((
        frame[..length as usize].to_vec(),
        fds,
        credentials.ok_or_else(denied)?,
    ))
}

/// Fixture-only loopback admission, guarded by ownership of a fresh user/net
/// namespace. This is never enabled by the shipping helper entry or UI.
#[cfg(any(test, feature = "test-support"))]
impl Session {
    pub fn in_test_namespace(control: OwnedFd, expected: Peer, host_net: &str) -> io::Result<Self> {
        verify_test_namespace(host_net)?;
        Self::create(control, expected, true)
    }
}

/// A caller-supplied different namespace name alone is insufficient: the net
/// namespace must be owned by this single-user namespace, not the host's.
#[cfg(any(test, feature = "test-support"))]
pub fn verify_test_namespace(host_net: &str) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let mapping = std::fs::read_to_string("/proc/self/uid_map")?;
    let fields: Vec<_> = mapping.split_whitespace().collect();
    if Peer::current().uid != 0
        || fields.len() != 3
        || fields[0] != "0"
        || fields[2] != "1"
        || std::fs::read_link("/proc/self/ns/net")?.to_string_lossy() == host_net
    {
        return Err(denied());
    }
    let network = std::fs::File::open("/proc/self/ns/net")?;
    // NS_GET_USERNS returns a new descriptor for the owning user namespace.
    // SAFETY: valid namespace FD; this ioctl takes no pointer argument.
    let owner = unsafe { libc::ioctl(network.as_raw_fd(), libc::NS_GET_USERNS) };
    if owner < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the returned descriptor is newly owned by this function.
    let owner = unsafe { std::fs::File::from_raw_fd(owner) }.metadata()?;
    let current = std::fs::metadata("/proc/self/ns/user")?;
    if owner.dev() != current.dev() || owner.ino() != current.ino() {
        return Err(denied());
    }
    Ok(())
}

#[cfg(test)]
#[path = "linux_session/tests.rs"]
mod tests;
