use super::*;
use std::os::unix::process::CommandExt;

fn pair() -> (OwnedFd, OwnedFd) {
    let mut pair = [-1; 2];
    // SAFETY: kernel fills exactly two output descriptor slots.
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                pair.as_mut_ptr(),
            )
        },
        0
    );
    // SAFETY: both returned descriptors are newly owned and distinct.
    unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) }
}
fn ready(client: &OwnedFd) {
    let mut bytes = [0; 4];
    // SAFETY: live exact-size output buffer and connected socket.
    assert_eq!(
        unsafe {
            libc::recv(
                client.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                0,
            )
        },
        4
    );
    assert_eq!(&bytes, READY);
}
fn setup() -> (OwnedFd, Session) {
    let (client, server) = pair();
    let session = Session::new(server, Peer::current()).unwrap();
    ready(&client);
    (client, session)
}
fn command(op: u8, id: u64) -> Vec<u8> {
    let mut frame = READY.to_vec();
    frame.push(op);
    frame.extend_from_slice(&id.to_be_bytes());
    frame
}
fn registration(id: u64) -> Vec<u8> {
    let mut frame = command(REGISTER, id);
    frame.push(4);
    frame.extend_from_slice(&[1, 1, 1, 1]);
    frame.extend_from_slice(&[0; 12]);
    frame.extend_from_slice(&50000u16.to_be_bytes());
    frame.extend_from_slice(&[0, 1, 0, 70, 0x12, 0x34, 0x56, 0x78]);
    frame.extend_from_slice(&[0; 66]);
    assert_eq!(frame.len(), MAX_FRAME);
    frame
}
fn send(client: &OwnedFd, frame: &[u8], fds: &[RawFd]) {
    let mut ancillary = [0u64; 40];
    let mut vector = libc::iovec {
        iov_base: frame.as_ptr().cast_mut().cast(),
        iov_len: frame.len(),
    };
    // SAFETY: inputs and aligned ancillary storage cover every declared byte.
    unsafe {
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        if !fds.is_empty() {
            message.msg_control = ancillary.as_mut_ptr().cast();
            message.msg_controllen = libc::CMSG_SPACE(std::mem::size_of_val(fds) as _) as usize;
            assert!(message.msg_controllen <= size_of_val(&ancillary));
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of_val(fds) as _) as usize;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr().cast::<u8>(),
                libc::CMSG_DATA(header),
                std::mem::size_of_val(fds),
            );
        }
        assert_eq!(
            libc::sendmsg(client.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
            frame.len() as isize
        );
    }
}
fn udp() -> UdpSocket {
    UdpSocket::bind("127.0.0.1:0").unwrap()
}
fn registered(event: Event) -> Lease {
    match event {
        Event::Registered(lease) => lease,
        _ => panic!("registered event expected"),
    }
}
fn next(session: &mut Session) -> io::Result<Event> {
    session.next(Some(Duration::from_secs(2)))
}

#[test]
fn authenticates_exact_pid_uid_and_seqpacket_before_ready() {
    for peer in [
        Peer {
            pid: Peer::current().pid + 1,
            ..Peer::current()
        },
        Peer {
            uid: Peer::current().uid.wrapping_add(1),
            ..Peer::current()
        },
    ] {
        let (_client, server) = pair();
        assert_eq!(
            Session::new(server, peer).err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
    let (client, mut session) = setup();
    send(&client, &command(STOP, 0), &[]);
    assert!(matches!(next(&mut session).unwrap(), Event::Stopped));
}

#[test]
fn cancel_invalidates_but_keeps_source_port_pinned_until_consumer_cleanup() {
    let (client, mut session) = setup();
    let flow = udp();
    let local = flow.local_addr().unwrap();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    drop(flow);
    let lease = registered(next(&mut session).unwrap());
    assert_eq!(lease.local(), local);
    assert_ne!(lease.cookie(), 0);
    assert_eq!(
        &lease.request()[..8],
        &[0, 1, 0, 70, 0x12, 0x34, 0x56, 0x78]
    );
    lease.validate().unwrap();
    assert_eq!(udp_bind_error(local), io::ErrorKind::AddrInUse);
    send(&client, &command(CANCEL, 1), &[]);
    assert!(matches!(next(&mut session).unwrap(), Event::Retired(1)));
    assert!(!lease.is_active());
    assert!(lease.validate().is_err());
    assert_eq!(
        udp_bind_error(local),
        io::ErrorKind::AddrInUse,
        "consumer still owns its rule/FD lease"
    );
    let newer = udp();
    send(&client, &registration(2), &[newer.as_raw_fd()]);
    let new_lease = registered(next(&mut session).unwrap());
    drop(lease); // The fixture has now removed its old rule.
    UdpSocket::bind(local).unwrap();
    assert!(
        new_lease.is_active(),
        "old cleanup cannot revoke replacement"
    );
    drop(session);
    assert!(!new_lease.is_active());
}
fn udp_bind_error(local: SocketAddr) -> io::ErrorKind {
    UdpSocket::bind(local).unwrap_err().kind()
}

#[test]
fn every_rejected_ancillary_descriptor_is_closed_including_truncation() {
    for count in [0, 2, 32] {
        let (client, mut session) = setup();
        let flows: Vec<_> = (0..count).map(|_| udp()).collect();
        let locals: Vec<_> = flows
            .iter()
            .map(|flow| flow.local_addr().unwrap())
            .collect();
        let rights: Vec<_> = flows.iter().map(AsRawFd::as_raw_fd).collect();
        send(&client, &registration(1), &rights);
        drop(flows);
        assert!(next(&mut session).is_err());
        for local in locals {
            UdpSocket::bind(local).unwrap();
        }
    }
    let (client, mut session) = setup();
    let flow = udp();
    let local = flow.local_addr().unwrap();
    let mut oversized = registration(1);
    oversized.push(0);
    send(&client, &oversized, &[flow.as_raw_fd()]);
    drop(flow);
    assert!(next(&mut session).is_err());
    UdpSocket::bind(local).unwrap();
}

#[test]
fn rejects_non_socket_tcp_unbound_reusable_wrong_family_and_private_destination() {
    let file = std::fs::File::open("/dev/null").unwrap();
    let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // SAFETY: socket returns one new descriptor, immediately owned below.
    let unbound = unsafe {
        OwnedFd::from_raw_fd(libc::socket(
            libc::AF_INET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            0,
        ))
    };
    let reused = udp();
    let yes: i32 = 1;
    // SAFETY: immutable initialized option input and live socket.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                reused.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEADDR,
                (&yes as *const i32).cast(),
                size_of::<i32>() as _,
            )
        },
        0
    );
    for fd in [
        file.as_raw_fd(),
        tcp.as_raw_fd(),
        unbound.as_raw_fd(),
        reused.as_raw_fd(),
    ] {
        let (client, mut session) = setup();
        send(&client, &registration(1), &[fd]);
        assert!(next(&mut session).is_err());
    }
    for change in [0, 1, 2, 3] {
        let (client, mut session) = setup();
        let flow = udp();
        let mut frame = registration(1);
        match change {
            0 => frame[14..18].copy_from_slice(&[127, 0, 0, 1]),
            1 => frame[32] = 0x80,
            2 => frame[13] = 6,
            _ => frame[30..32].fill(0),
        }
        send(&client, &frame, &[flow.as_raw_fd()]);
        assert!(next(&mut session).is_err());
    }
}

#[test]
fn cap_replay_and_bad_command_retire_existing_leases() {
    for rejected in [0, 1, 5] {
        let (client, mut session) = setup();
        let mut leases = Vec::new();
        let mut flows = Vec::new();
        for id in 1..=4 {
            let flow = udp();
            send(&client, &registration(id), &[flow.as_raw_fd()]);
            leases.push(registered(next(&mut session).unwrap()));
            flows.push(flow);
        }
        let extra = udp();
        send(&client, &registration(rejected), &[extra.as_raw_fd()]);
        assert!(next(&mut session).is_err());
        assert!(leases.iter().all(|lease| !lease.is_active()));
        assert_eq!(session.leases.len(), 0);
    }
    let (client, mut session) = setup();
    let flow = udp();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    send(&client, &command(99, 1), &[]);
    assert!(next(&mut session).is_err());
    assert!(!lease.is_active());
}

#[test]
fn absolute_deadline_and_disconnect_retire_leases_without_refilling() {
    let (client, mut session) = setup();
    let flow = udp();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    let until = lease.0.until;
    assert!(session.expire(until - Duration::from_nanos(1)).is_none());
    assert!(matches!(session.expire(until), Some(Event::Retired(1))));
    assert!(
        !lease.is_active(),
        "expiry invalidates the consumer's actual lease"
    );
    drop(lease);
    send(&client, &registration(2), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    drop(client);
    assert!(next(&mut session).is_err());
    assert!(!lease.is_active());
}

#[test]
fn explicit_stop_retires_every_lease_and_duplicate_socket_is_refused() {
    let (client, mut session) = setup();
    let flow = udp();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    send(&client, &command(STOP, 0), &[]);
    assert!(matches!(next(&mut session).unwrap(), Event::Stopped));
    assert!(!lease.is_active());
    let (client, mut session) = setup();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    send(&client, &registration(2), &[flow.as_raw_fd()]);
    assert!(next(&mut session).is_err());
    assert!(!lease.is_active());
}

// A subprocess deliberately writes on its parent's inherited IPC descriptor.
// Connection-level SO_PEERCRED alone would accept it; SCM_CREDENTIALS must not.
#[test]
fn same_uid_delegated_ipc_sender_cannot_impersonate_the_connected_parent() {
    let (client, mut session) = setup();
    let flow = udp();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    let raw = client.as_raw_fd();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "network::linux_session::tests::subprocess_sender",
            "--nocapture",
        ])
        .env("FASTCORD_SESSION_TEST_FD", raw.to_string());
    // SAFETY: pre_exec performs only the async-signal-safe fcntl syscall;
    // close-on-exec changes occur solely in this child, never other test spawns.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(raw, libc::F_SETFD, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    assert_eq!(
        next(&mut session).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(!lease.is_active());
    assert!(child.wait().unwrap().success());
}
#[test]
fn subprocess_sender() {
    let Ok(fd) = std::env::var("FASTCORD_SESSION_TEST_FD") else {
        return;
    };
    let fd: i32 = fd.parse().unwrap();
    // SAFETY: explicit descriptor inherited solely by this subprocess fixture.
    let control = unsafe { OwnedFd::from_raw_fd(fd) };
    send(&control, &command(STOP, 0), &[]);
}

#[test]
fn canceled_cookie_cannot_be_readmitted_before_old_rule_owner_releases_it() {
    let (client, mut session) = setup();
    let flow = udp();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let old = registered(next(&mut session).unwrap());
    send(&client, &command(CANCEL, 1), &[]);
    assert!(matches!(next(&mut session).unwrap(), Event::Retired(1)));
    send(&client, &registration(2), &[flow.as_raw_fd()]);
    assert!(
        next(&mut session).is_err(),
        "same tuple is still owned by retired consumer"
    );
    assert!(!old.is_active());
}

fn address(name: &str) -> (libc::sockaddr_un, libc::socklen_t) {
    // SAFETY: zero is valid empty sockaddr_un storage.
    let mut address: libc::sockaddr_un = unsafe { zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    assert!(name.len() + 1 < address.sun_path.len());
    for (to, from) in address.sun_path[1..].iter_mut().zip(name.bytes()) {
        *to = from as _;
    }
    let length = std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + name.len();
    (address, length as _)
}
fn seqsocket() -> OwnedFd {
    // SAFETY: syscall has no pointer arguments and returns one owned FD.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0) };
    assert!(fd >= 0);
    unsafe { OwnedFd::from_raw_fd(fd) }
}
fn connect(name: &str) -> OwnedFd {
    let socket = seqsocket();
    let (address, length) = address(name);
    // SAFETY: initialized sockaddr and its exact bounded length.
    assert_eq!(
        unsafe {
            libc::connect(
                socket.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        },
        0
    );
    socket
}

#[test]
fn peer_exit_retires_leases_even_while_another_process_holds_its_ipc_socket() {
    use std::io::Write;
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = format!(
        "fastcord-session-test-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    );
    let listener = seqsocket();
    let (address, length) = address(&name);
    // SAFETY: initialized address, valid live listener FD.
    assert_eq!(
        unsafe {
            libc::bind(
                listener.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                length,
            )
        },
        0
    );
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 1) }, 0);
    let (side, child_side) = pair();
    let one: i32 = 1;
    assert_eq!(
        unsafe {
            libc::setsockopt(
                side.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&one as *const i32).cast(),
                size_of::<i32>() as _,
            )
        },
        0
    );
    let raw = child_side.as_raw_fd();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "network::linux_session::tests::subprocess_connected_owner",
            "--nocapture",
        ])
        .env("FASTCORD_SESSION_CONNECT", &name)
        .env("FASTCORD_SESSION_SIDE_FD", raw.to_string())
        .stdin(std::process::Stdio::piped());
    // SAFETY: only async-signal-safe fcntl in this child's pre-exec path.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(raw, libc::F_SETFD, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    let mut child = command.spawn().unwrap();
    drop(child_side);
    // SAFETY: null address outputs are permitted; returned descriptor is owned.
    let accepted = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    assert!(accepted >= 0);
    let mut session = Session::new(
        unsafe { OwnedFd::from_raw_fd(accepted) },
        Peer {
            pid: child.id(),
            ..Peer::current()
        },
    )
    .unwrap();
    let lease = registered(next(&mut session).unwrap());
    let (_, held, _) = receive(side.as_raw_fd()).unwrap();
    assert_eq!(held.len(), 1);
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        next(&mut session).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(!lease.is_active());
    drop(held);
}

#[test]
fn subprocess_connected_owner() {
    let Ok(name) = std::env::var("FASTCORD_SESSION_CONNECT") else {
        return;
    };
    let control = connect(&name);
    let side: i32 = std::env::var("FASTCORD_SESSION_SIDE_FD")
        .unwrap()
        .parse()
        .unwrap();
    // SAFETY: explicit child-only inherited fixture endpoint.
    let side = unsafe { OwnedFd::from_raw_fd(side) };
    ready(&control);
    let flow = udp();
    send(&control, &registration(1), &[flow.as_raw_fd()]);
    send(&side, b"hold", &[control.as_raw_fd()]);
    let mut exit = String::new();
    std::io::stdin().read_line(&mut exit).unwrap();
}

#[test]
fn fixture_loopback_bypass_refuses_the_host_namespace() {
    let host = std::fs::read_link("/proc/self/ns/net").unwrap();
    assert!(verify_test_namespace(&host.to_string_lossy()).is_err());
    let (_client, server) = pair();
    assert!(Session::in_test_namespace(server, Peer::current(), &host.to_string_lossy()).is_err());
}

#[test]
fn reusable_port_and_changed_connected_peer_are_refused() {
    let (client, mut session) = setup();
    let reusable = udp();
    let one: i32 = 1;
    // SAFETY: initialized integer input and owned socket.
    assert_eq!(
        unsafe {
            libc::setsockopt(
                reusable.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_REUSEPORT,
                (&one as *const i32).cast(),
                size_of::<i32>() as _,
            )
        },
        0
    );
    send(&client, &registration(1), &[reusable.as_raw_fd()]);
    assert!(next(&mut session).is_err());
    let (client, mut session) = setup();
    let flow = udp();
    send(&client, &registration(1), &[flow.as_raw_fd()]);
    let lease = registered(next(&mut session).unwrap());
    flow.connect("127.0.0.1:50001").unwrap(); // local route lookup only, no datagram
    assert!(lease.validate().is_err());
}

#[test]
fn fatal_protocol_error_closes_already_queued_ancillary_rights() {
    let (client, mut session) = setup();
    let first = udp();
    let second = udp();
    let first_address = first.local_addr().unwrap();
    let second_address = second.local_addr().unwrap();
    send(&client, &command(99, 1), &[first.as_raw_fd()]);
    send(&client, &registration(2), &[second.as_raw_fd()]);
    drop(first);
    drop(second);
    assert!(next(&mut session).is_err());
    UdpSocket::bind(first_address).unwrap();
    UdpSocket::bind(second_address).unwrap();
}

#[test]
fn late_cancel_after_retirement_cannot_revoke_another_lease() {
    let (client, mut session) = setup();
    let first = udp();
    let second = udp();
    send(&client, &registration(1), &[first.as_raw_fd()]);
    let old = registered(next(&mut session).unwrap());
    send(&client, &registration(2), &[second.as_raw_fd()]);
    let current = registered(next(&mut session).unwrap());
    assert!(matches!(
        session.expire(old.0.until),
        Some(Event::Retired(1))
    ));
    send(&client, &command(CANCEL, 1), &[]);
    assert!(matches!(next(&mut session).unwrap(), Event::Retired(1)));
    assert!(current.is_active());
    assert!(!old.is_active());
}
