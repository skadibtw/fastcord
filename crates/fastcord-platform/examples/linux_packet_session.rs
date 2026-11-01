//! Guarded admission fixture for scripts/test_linux_packet_session.py.
//! No firewall commands or raw packet operations exist in this executable.
#![allow(unsafe_code)]

#[cfg(target_os = "linux")]
fn main() -> std::io::Result<()> {
    use fastcord_platform::network::linux_session::{Event, Peer, Session, verify_test_namespace};
    use std::io::{self, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 6 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    verify_test_namespace(&args[1])?;
    let invalid = |_| io::Error::from(io::ErrorKind::InvalidInput);
    let listener: i32 = args[2].parse().map_err(invalid)?;
    let expected = Peer {
        pid: args[3].parse().map_err(invalid)?,
        uid: args[4].parse().map_err(invalid)?,
        gid: args[5].parse().map_err(invalid)?,
    };
    if listener < 0 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    // SAFETY: guarded fixture receives one explicit owned inherited listener.
    let listener = unsafe { OwnedFd::from_raw_fd(listener) };
    // SAFETY: live socket; null address outputs are allowed. The new FD is owned.
    let accepted = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if accepted < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut session = Session::in_test_namespace(
        unsafe { OwnedFd::from_raw_fd(accepted) },
        expected,
        &args[1],
    )?;
    let mut held = None;
    loop {
        match session.next(None) {
            Ok(Event::Registered(lease)) => {
                lease.validate()?;
                println!(
                    "registered {} {} {} {}",
                    lease.id(),
                    lease.local().port(),
                    lease.peer().port(),
                    lease.cookie()
                );
                io::stdout().flush()?;
                held = Some(lease);
            }
            event => {
                match event {
                    Ok(Event::Retired(id)) => println!("retired {id}"),
                    Ok(Event::Stopped) => println!("stopped"),
                    Err(error) => println!("ended {:?}", error.kind()),
                    Ok(Event::Registered(_)) => unreachable!(),
                }
                io::stdout().flush()?;
                // The harness owns the rule and retains the original FD too.
                // Wait for explicit cleanup before releasing the consumer lease.
                let until = std::time::Instant::now() + std::time::Duration::from_secs(2);
                let mut answer = [0u8; 8];
                let mut used = 0;
                while used < answer.len() {
                    let left = until.saturating_duration_since(std::time::Instant::now());
                    if left.is_zero() {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    let mut ready = libc::pollfd {
                        fd: 0,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: one writable pollfd and a fixed live output slice;
                    // each read occurs only after readiness within one deadline.
                    if unsafe { libc::poll(&mut ready, 1, left.as_millis().max(1) as i32) } != 1 {
                        return Err(io::ErrorKind::TimedOut.into());
                    }
                    let read = unsafe {
                        libc::read(0, answer[used..].as_mut_ptr().cast(), answer.len() - used)
                    };
                    if read <= 0 {
                        return Err(io::ErrorKind::UnexpectedEof.into());
                    }
                    used += read as usize;
                }
                if &answer != b"removed\n" {
                    return Err(io::ErrorKind::InvalidData.into());
                }
                drop(held.take());
                return Ok(());
            }
        }
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("Linux namespace fixture is unsupported on this target");
}
