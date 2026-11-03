//! The single-instance guard (issue #105). Two fastcord processes on one token
//! are never started: the first process holds a per-session named mutex and
//! owns a hidden message-only window; a later launch finds that window, hands
//! its command-line argument over with `WM_COPYDATA`, and tells the caller to
//! exit. The window thread blocks in `GetMessageW`, so the guard costs no idle
//! CPU. Linux and macOS hold an exclusive `flock` on a lock file in a private
//! per-user directory instead (the kernel drops it with a crashed process, so
//! staleness is never guessed); the holder listens on a Unix socket beside it
//! (a thread blocked in `accept`), a later launch connects and writes its
//! argument. Where no guard can be set up, [`acquire`] reports a primary
//! instance without one rather than exiting silently.

#[cfg(windows)]
use std::thread::JoinHandle;

/// Longest forwarded argument, in bytes (a deep link is far shorter).
pub const MAX_ARG_BYTES: usize = 2048;
/// `COPYDATASTRUCT::dwData` tag of a fastcord hand-off ("FC" + version 1);
/// the Unix socket has no tag field and passes it implicitly.
const TAG: usize = 0x4643_0001;

/// What a second launch asked the running instance to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Forwarded {
    /// Bring the window forward.
    Raise,
    /// Bring the window forward and open this link.
    Open(String),
}

/// The payload for a hand-off. An absent or empty argument means "raise". A
/// longer-than-allowed argument, or one with control characters, is refused:
/// the receiver would drop it anyway.
pub fn encode(arg: Option<&str>) -> Option<Vec<u8>> {
    let arg = arg.unwrap_or("");
    valid(arg.as_bytes()).then(|| arg.as_bytes().to_vec())
}

/// The request in a received message, or `None` for anything that is not an
/// exact fastcord hand-off (another program's message, oversize, not UTF-8).
pub fn decode(tag: usize, payload: &[u8]) -> Option<Forwarded> {
    if tag != TAG || !valid(payload) {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    Some(if text.is_empty() {
        Forwarded::Raise
    } else {
        Forwarded::Open(text.to_owned())
    })
}

fn valid(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_ARG_BYTES
        && std::str::from_utf8(bytes).is_ok_and(|text| !text.chars().any(char::is_control))
}

/// The running instance's guard; keep it alive until the process exits.
pub struct Instance {
    #[cfg(windows)]
    thread: Option<(u32, JoinHandle<()>)>,
    /// The socket file, removed when the guard is dropped.
    #[cfg(unix)]
    socket: Option<std::path::PathBuf>,
    /// The locked file: whoever holds it is the primary (#138).
    #[cfg(unix)]
    _lock: Option<std::fs::File>,
}

pub enum Start {
    /// This process is the (only) instance; forwarded requests reach the sink.
    Primary(Instance),
    /// Another instance runs; `arg` was handed to it (or could not be). The
    /// caller must exit without starting a session.
    Secondary,
}

/// Becomes the primary instance or hands `arg` to the one that exists. The
/// sink runs on the guard's thread and must only enqueue.
pub fn acquire(arg: Option<&str>, sink: impl FnMut(Forwarded) + Send + 'static) -> Start {
    sys::acquire(arg, Box::new(sink))
}

#[cfg(windows)]
#[allow(unsafe_code)] // Win32 mutex and window FFI, confined here.
mod sys {
    use std::cell::RefCell;
    use std::sync::mpsc;
    use std::time::Duration;

    use windows::Win32::Foundation::{
        ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
    };
    use windows::Win32::System::DataExchange::COPYDATASTRUCT;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        ASFW_ANY, AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow,
        FindWindowExW, GetMessageW, HWND_MESSAGE, MSG, PostThreadMessageW, RegisterClassW,
        SMTO_ABORTIFHUNG, SendMessageTimeoutW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COPYDATA, WM_QUIT,
        WNDCLASSW,
    };
    use windows::core::{PCWSTR, w};

    use super::{Forwarded, Instance, Start, TAG, decode, encode};

    type Sink = Box<dyn FnMut(Forwarded) + Send>;

    const CLASS: PCWSTR = w!("FastcordSingleInstance");

    thread_local! {
        static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    }

    pub fn acquire(arg: Option<&str>, sink: Sink) -> Start {
        // The mutex lives for the whole process: it is deliberately never closed.
        // SAFETY: plain named-mutex creation; the name is a NUL-terminated literal.
        let mutex = unsafe { CreateMutexW(None, false, w!(r"Local\fastcord-single-instance")) };
        // SAFETY: reads the calling thread's last error, set by the call above.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if mutex.is_err() {
            // Without the mutex nothing proves we are alone; refuse to run a second session.
            return Start::Secondary;
        }
        if exists {
            forward(arg);
            return Start::Secondary;
        }
        let (ready, started) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("fastcord-instance".into())
            .spawn(move || run(sink, ready))
            .ok();
        let id = started.recv().ok().flatten();
        Start::Primary(Instance {
            thread: thread.zip(id).map(|(thread, id)| (id, thread)),
        })
    }

    /// Delivers `arg` to the primary's window, waiting briefly for a window that
    /// is still being created. A failure is not retried further: the caller
    /// exits either way, because starting a second session is the worse outcome.
    fn forward(arg: Option<&str>) {
        let Some(payload) = encode(arg) else { return };
        for _ in 0..20 {
            // SAFETY: looks up a top-level message-only window by class name.
            let found = unsafe { FindWindowExW(Some(HWND_MESSAGE), None, CLASS, None) };
            if let Ok(window) = found {
                let data = COPYDATASTRUCT {
                    dwData: TAG,
                    cbData: payload.len() as u32,
                    lpData: payload.as_ptr() as *mut _,
                };
                // SAFETY: `data` and `payload` outlive the synchronous send; the
                // receiver only reads them. The foreground right lets the primary
                // raise its window for this launch.
                unsafe {
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                    let _ = SendMessageTimeoutW(
                        window,
                        WM_COPYDATA,
                        WPARAM(0),
                        LPARAM(&data as *const _ as isize),
                        SMTO_ABORTIFHUNG,
                        2000,
                        None,
                    );
                }
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn run(sink: Sink, ready: mpsc::SyncSender<Option<u32>>) {
        // SAFETY: registers a class whose procedure is a valid `extern "system"` fn
        // and creates a message-only window owned by this thread.
        let window = unsafe {
            let module = GetModuleHandleW(None).ok().map(Into::into);
            let class = WNDCLASSW {
                lpfnWndProc: Some(procedure),
                hInstance: module.unwrap_or_default(),
                lpszClassName: CLASS,
                ..Default::default()
            };
            RegisterClassW(&class);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                w!("fastcord"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                module,
                None,
            )
        };
        let Ok(window) = window else {
            let _ = ready.send(None);
            return;
        };
        SINK.with(|slot| *slot.borrow_mut() = Some(sink));
        // SAFETY: reads the calling thread's id.
        let _ = ready.send(Some(unsafe { GetCurrentThreadId() }));
        let mut message = MSG::default();
        // SAFETY: blocks until a message arrives: no CPU while nothing is forwarded.
        while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
            // SAFETY: the window procedure below handles every message of this window.
            unsafe { windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&message) };
        }
        // SAFETY: destroys the window created above on its own thread.
        let _ = unsafe { DestroyWindow(window) };
    }

    unsafe extern "system" fn procedure(
        window: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_COPYDATA && lparam.0 != 0 {
            // SAFETY: for WM_COPYDATA `lparam` points to a COPYDATASTRUCT that is
            // valid for the duration of the call; the length is checked before use.
            let request = unsafe {
                let data = &*(lparam.0 as *const COPYDATASTRUCT);
                if data.cbData as usize <= super::MAX_ARG_BYTES
                    && (data.cbData == 0 || !data.lpData.is_null())
                {
                    let bytes = if data.cbData == 0 {
                        &[][..]
                    } else {
                        std::slice::from_raw_parts(data.lpData as *const u8, data.cbData as usize)
                    };
                    decode(data.dwData, bytes)
                } else {
                    None
                }
            };
            if let Some(request) = request {
                SINK.with(|sink| {
                    if let Ok(mut sink) = sink.try_borrow_mut()
                        && let Some(sink) = sink.as_mut()
                    {
                        sink(request);
                    }
                });
                return LRESULT(1);
            }
            return LRESULT(0);
        }
        // SAFETY: default handling of every other message.
        unsafe { DefWindowProcW(window, message, wparam, lparam) }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if let Some((id, thread)) = self.thread.take() {
                // SAFETY: posts a plain quit to the guard thread, joined below.
                unsafe {
                    let _ = PostThreadMessageW(id, WM_QUIT, WPARAM(0), LPARAM(0));
                }
                let _ = thread.join();
            }
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::io::{self, Read, Write};
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use super::{Forwarded, Instance, MAX_ARG_BYTES, Start, TAG, decode, encode};

    type Sink = Box<dyn FnMut(Forwarded) + Send>;

    /// How long a connected launch may take to write its argument.
    const READ_WAIT: Duration = Duration::from_secs(1);
    /// How long a second launch waits for the lock holder's socket to appear.
    const BIND_WAIT: Duration = Duration::from_secs(2);

    /// The socket in a directory only this user can reach: `XDG_RUNTIME_DIR`
    /// on Linux, the per-user `TMPDIR` on macOS, else `~/.cache/fastcord`
    /// (made 0700). Never a shared directory such as `/tmp` (#138).
    fn socket_path() -> Option<PathBuf> {
        let runtime = if cfg!(target_os = "macos") {
            "TMPDIR"
        } else {
            "XDG_RUNTIME_DIR"
        };
        let dir = std::env::var_os(runtime)
            .and_then(|dir| private_location(Path::new(&dir), user_id()))
            .or_else(private_cache)?;
        Some(dir.join(".fastcord-instance.sock"))
    }

    fn private_cache() -> Option<PathBuf> {
        use std::os::unix::fs::DirBuilderExt;
        let home = PathBuf::from(std::env::var_os("HOME")?);
        if !home.is_absolute() || std::fs::metadata(&home).ok()?.uid() != user_id() {
            return None;
        }
        let dir = home.join(".cache").join("fastcord");
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir);
        private_location(&dir, user_id())
    }

    #[allow(unsafe_code)]
    fn user_id() -> u32 {
        // SAFETY: geteuid takes no pointers and has no preconditions.
        unsafe { libc::geteuid() }
    }

    /// Return a canonical private endpoint under ancestors that another user
    /// cannot replace. Keep this returned path for all subsequent operations,
    /// so changing a symlink in the original prefix cannot redirect them.
    fn private_location(dir: &Path, owner: u32) -> Option<PathBuf> {
        // `symlink/` follows the final link even for lstat on Unix.
        let normalized: PathBuf = dir.components().collect();
        if !dir.is_absolute()
            || dir
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        {
            return None;
        }
        let meta = std::fs::symlink_metadata(&normalized).ok()?;
        if !meta.is_dir() || meta.uid() != owner || meta.mode() & 0o077 != 0 {
            return None;
        }
        let canonical = normalized.canonicalize().ok()?;
        let ancestors: Vec<_> = canonical.ancestors().collect();
        // Walk from root downward. Root/the current user are trusted; a sticky
        // root-owned /tmp is safe, but a foreign owner or writable non-sticky
        // ancestor could replace even a private child directory.
        for ancestor in ancestors.into_iter().rev() {
            let meta = std::fs::symlink_metadata(ancestor).ok()?;
            if !meta.is_dir()
                || (meta.uid() != 0 && meta.uid() != owner)
                || (meta.mode() & 0o022 != 0 && meta.mode() & 0o1000 == 0)
            {
                return None;
            }
            if ancestor == canonical && (meta.uid() != owner || meta.mode() & 0o077 != 0) {
                return None;
            }
        }
        Some(canonical)
    }

    #[cfg(test)]
    pub(super) fn private_directory(dir: &Path, owner: u32) -> bool {
        private_location(dir, owner).is_some()
    }

    pub fn acquire(arg: Option<&str>, sink: Sink) -> Start {
        match socket_path() {
            Some(path) => acquire_at(&path, arg, sink),
            // Nowhere private to put a guard: run, rather than exit silently.
            None => Start::Primary(Instance {
                socket: None,
                _lock: None,
            }),
        }
    }

    pub(super) fn acquire_at(path: &Path, arg: Option<&str>, sink: Sink) -> Start {
        use std::os::unix::fs::OpenOptionsExt;
        let location = path
            .parent()
            .and_then(|dir| private_location(dir, user_id()));
        let Some(location) = location else {
            return Start::Primary(Instance {
                socket: None,
                _lock: None,
            });
        };
        let Some(name) = path.file_name() else {
            return Start::Primary(Instance {
                socket: None,
                _lock: None,
            });
        };
        let canonical_path = location.join(name);
        let path = canonical_path.as_path();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path.with_extension("lock"));
        let lock = lock.ok().filter(|file| {
            file.metadata().is_ok_and(|meta| {
                meta.is_file()
                    && meta.uid() == user_id()
                    && meta.mode() & 0o077 == 0
                    && meta.nlink() == 1
            })
        });
        let Some(lock) = lock else {
            // The private directory refused the file: run unguarded rather
            // than exit with no window.
            return Start::Primary(Instance {
                socket: None,
                _lock: None,
            });
        };
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                // The holder may not have bound its socket yet.
                let deadline = std::time::Instant::now() + BIND_WAIT;
                loop {
                    if let Ok(stream) = UnixStream::connect(path) {
                        forward(stream, arg);
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                return Start::Secondary;
            }
            Err(std::fs::TryLockError::Error(_)) => {
                return Start::Primary(Instance {
                    socket: None,
                    _lock: None,
                });
            }
        }
        // Holding the lock, any socket file is a dead one's.
        let _ = std::fs::remove_file(path);
        let socket = UnixListener::bind(path).ok().and_then(|listener| {
            std::thread::Builder::new()
                .name("fastcord-instance".into())
                .spawn(move || serve(listener, sink))
                .ok()
                .map(|_| path.to_owned())
        });
        Start::Primary(Instance {
            socket,
            _lock: Some(lock),
        })
    }

    /// Writes `arg` to the primary. A failure is not retried: the caller exits
    /// either way, because starting a second session is the worse outcome.
    fn forward(mut stream: UnixStream, arg: Option<&str>) {
        if let Some(payload) = encode(arg) {
            let _ = stream.set_write_timeout(Some(READ_WAIT));
            let _ = stream.write_all(&payload);
        }
    }

    /// Blocks in `accept`: no CPU while nothing is forwarded. The thread ends
    /// with the process.
    fn serve(listener: UnixListener, mut sink: Sink) {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if let Ok(payload) = read_handoff(stream)
                && let Some(request) = decode(TAG, &payload)
            {
                sink(request);
            }
        }
    }

    /// One deadline for the complete request, including successful partial
    /// reads. A peer sending one byte at a time cannot renew its admission.
    pub(super) fn read_handoff(mut stream: UnixStream) -> io::Result<Vec<u8>> {
        let deadline = Instant::now() + READ_WAIT;
        // Darwin may reject setsockopt after the peer has closed, even while
        // its payload remains buffered. Nonblocking reads drain those bytes;
        // poll bounds waiting without reconfiguring a disconnected socket.
        stream.set_nonblocking(true)?;
        let mut payload = Vec::with_capacity(MAX_ARG_BYTES + 1);
        let mut chunk = [0; MAX_ARG_BYTES + 1];
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let available = MAX_ARG_BYTES + 1 - payload.len();
            match stream.read(&mut chunk[..available]) {
                Ok(0) => return Ok(payload),
                Ok(count) => {
                    payload.extend_from_slice(&chunk[..count]);
                    if payload.len() > MAX_ARG_BYTES {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    wait_readable(&stream, deadline)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[allow(unsafe_code)]
    fn wait_readable(stream: &UnixStream, deadline: Instant) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let timeout = remaining
                .as_nanos()
                .div_ceil(1_000_000)
                .min(i32::MAX as u128) as i32;
            let mut fd = libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one initialized pollfd for a live borrowed socket, held
            // throughout this call; the wait is capped by the request deadline.
            match unsafe { libc::poll(&mut fd, 1, timeout) } {
                -1 => {
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                0 => continue,
                _ if fd.revents & libc::POLLNVAL != 0 => {
                    return Err(io::ErrorKind::InvalidInput.into());
                }
                // HUP/ERR must also reach read: buffered bytes precede EOF.
                _ => return Ok(()),
            }
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            if let Some(path) = self.socket.take() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::{Forwarded, Instance, Start};

    pub fn acquire(_: Option<&str>, _: Box<dyn FnMut(Forwarded) + Send>) -> Start {
        Start::Primary(Instance {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handoff_round_trips_and_an_empty_one_raises_the_window() {
        let link = "discord://-/channels/1/2/3";
        let payload = encode(Some(link)).unwrap();
        assert_eq!(
            decode(TAG, &payload),
            Some(Forwarded::Open(link.to_owned()))
        );
        assert_eq!(decode(TAG, &encode(None).unwrap()), Some(Forwarded::Raise));
        assert_eq!(
            decode(TAG, &encode(Some("")).unwrap()),
            Some(Forwarded::Raise)
        );
    }

    #[test]
    fn foreign_oversize_and_malformed_messages_are_dropped() {
        assert_eq!(decode(TAG + 1, b"x"), None, "another program's tag");
        assert_eq!(decode(TAG, &[0xff, 0xfe]), None, "not UTF-8");
        assert_eq!(decode(TAG, b"a\nb"), None, "control character");
        let long = "a".repeat(MAX_ARG_BYTES + 1);
        assert_eq!(decode(TAG, long.as_bytes()), None);
        assert_eq!(encode(Some(&long)), None);
        assert!(encode(Some(&"a".repeat(MAX_ARG_BYTES))).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn shared_foreign_relative_and_symlink_directories_are_refused() {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt, symlink};
        let root =
            std::env::temp_dir().join(format!("fastcord-instance-privacy-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let owner = std::fs::metadata(&root).unwrap().uid();
        assert!(sys::private_directory(&root, owner));
        assert!(!sys::private_directory(&root, owner.wrapping_add(1)));
        assert!(!sys::private_directory(std::path::Path::new("."), owner));
        let alias = root.join("alias");
        symlink(&root, &alias).unwrap();
        assert!(!sys::private_directory(&alias, owner));
        assert!(!sys::private_directory(&alias.join(""), owner));
        assert!(!sys::private_directory(&alias.join("."), owner));
        let writable_parent = root.join("shared");
        std::fs::create_dir(&writable_parent).unwrap();
        let private_child = writable_parent.join("private");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&private_child)
            .unwrap();
        std::fs::set_permissions(&writable_parent, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!sys::private_directory(&private_child, owner));
        // The sticky bit prevents foreign users from replacing an owned child.
        std::fs::set_permissions(&writable_parent, std::fs::Permissions::from_mode(0o1777))
            .unwrap();
        assert!(sys::private_directory(&private_child, owner));
        let path = root.join("i.sock");
        // A shared runtime directory must be rejected before its existing
        // socket or lock is read, removed, or overwritten.
        std::fs::write(&path, b"foreign sentinel").unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        let Start::Primary(guard) = sys::acquire_at(&path, None, Box::new(|_| {})) else {
            panic!("unguarded fallback");
        };
        assert!(guard.socket.is_none());
        assert_eq!(std::fs::read(&path).unwrap(), b"foreign sentinel");
        assert!(!path.with_extension("lock").exists());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn nonprivate_or_linked_lock_files_are_not_used() {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
        let root =
            std::env::temp_dir().join(format!("fastcord-instance-lock-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root)
            .unwrap();
        let path = root.join("i.sock");
        let lock = path.with_extension("lock");
        let target = root.join("target");
        std::fs::write(&target, b"unchanged").unwrap();
        symlink(&target, &lock).unwrap();
        for case in 0..3 {
            let Start::Primary(guard) = sys::acquire_at(&path, None, Box::new(|_| {})) else {
                panic!("unguarded fallback");
            };
            assert!(guard.socket.is_none());
            assert_eq!(std::fs::read(&target).unwrap(), b"unchanged");
            assert!(!path.exists());
            std::fs::remove_file(&lock).unwrap();
            if case == 0 {
                std::fs::write(&lock, b"private ownership required").unwrap();
                std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o644)).unwrap();
            } else if case == 1 {
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
                std::fs::hard_link(&target, &lock).unwrap();
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn buffered_handoffs_survive_sender_close_before_reading() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        for payload in [
            Vec::new(),
            b"discord://-/channels/1/2".to_vec(),
            vec![b'x'; MAX_ARG_BYTES],
        ] {
            let (receiver, mut sender) = UnixStream::pair().unwrap();
            sender.write_all(&payload).unwrap();
            // Deterministically enter Darwin's disconnected-peer state before
            // receiving, while its payload and EOF remain available to read.
            drop(sender);
            assert_eq!(sys::read_handoff(receiver).unwrap(), payload);
        }
        let (receiver, mut sender) = UnixStream::pair().unwrap();
        sender.write_all(&vec![b'x'; MAX_ARG_BYTES + 1]).unwrap();
        drop(sender);
        assert_eq!(
            sys::read_handoff(receiver).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_slow_handoff_cannot_extend_the_total_read_deadline() {
        use std::io::Write;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let dir =
            std::env::temp_dir().join(format!("fastcord-instance-deadline-{}", std::process::id()));
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        }
        let path = dir.join("i.sock");
        let (tx, rx) = mpsc::channel();
        let Start::Primary(_guard) = sys::acquire_at(
            &path,
            None,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        ) else {
            panic!("first launch");
        };
        let mut slow = UnixStream::connect(&path).unwrap();
        slow.write_all(b"d").unwrap();
        let started = Instant::now();
        let writer = std::thread::spawn(move || {
            // Each interval is below the old per-read timeout. Keep the
            // connection alive well beyond the whole-request deadline.
            for _ in 0..60 {
                std::thread::sleep(Duration::from_millis(100));
                if slow.write_all(b"x").is_err() {
                    break;
                }
            }
        });
        let mut next = UnixStream::connect(&path).unwrap();
        next.write_all(b"discord://-/channels/1/2").unwrap();
        drop(next);
        let result = rx.recv_timeout(Duration::from_secs(4));
        let elapsed = started.elapsed();
        writer.join().unwrap();
        assert_eq!(
            result,
            Ok(Forwarded::Open("discord://-/channels/1/2".into())),
            "a slow partial handoff must be dropped before accepting the next"
        );
        assert!(elapsed < Duration::from_secs(4));
        drop(_guard);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_second_launch_hands_its_link_over_a_socket_and_a_stale_socket_is_replaced() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = std::env::temp_dir().join(format!("fastcord-instance-{}", std::process::id()));
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        }
        let path = dir.join("i.sock");
        // A socket file nobody listens on, as a crashed process leaves it.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        // Holding the lock decides, not the socket: while it is held, even a
        // missing socket makes a launch secondary (#138).

        let (tx, rx) = mpsc::channel();
        let Start::Primary(guard) = sys::acquire_at(
            &path,
            None,
            Box::new(move |r| {
                let _ = tx.send(r);
            }),
        ) else {
            panic!("the stale socket must not make this a second launch");
        };
        let link = "discord://-/channels/1/2";
        let ignore = |_| {};
        assert!(matches!(
            sys::acquire_at(&path, Some(link), Box::new(ignore)),
            Start::Secondary
        ));
        let wait = Duration::from_secs(5);
        assert_eq!(rx.recv_timeout(wait), Ok(Forwarded::Open(link.to_owned())));
        assert!(matches!(
            sys::acquire_at(&path, None, Box::new(ignore)),
            Start::Secondary
        ));
        assert_eq!(rx.recv_timeout(wait), Ok(Forwarded::Raise));
        drop(guard);
        assert!(!path.exists(), "the guard removes its socket");
        // The lock went with the guard: the next launch is the primary.
        let Start::Primary(next) = sys::acquire_at(&path, None, Box::new(ignore)) else {
            panic!("a released lock must make a new primary");
        };
        std::fs::remove_file(&path).unwrap();
        assert!(matches!(
            sys::acquire_at(&path, None, Box::new(ignore)),
            Start::Secondary
        ));
        drop(next);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
