use super::{Error, executable};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DESKTOP: &str = "io.github.skadibtw.fastcord-links.desktop";
const MIME: &str = "x-scheme-handler/fastcord";
const LIMIT: usize = 4096;

fn entry(exe: &Path) -> Result<String, Error> {
    let exe = executable(exe)?;
    if exe.contains('\\') {
        return Err(Error::UnsafeExecutable);
    }
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=fastcord links\nNoDisplay=true\nTerminal=false\nExec=\"{exe}\" --open-url %u\nMimeType={MIME};\nX-Fastcord-Link-Owner=1\n"
    ))
}

pub(super) fn register(exe: &Path) -> Result<(), Error> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute());
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|p| p.join(".local/share")))
        .ok_or(Error::Failed)?;
    let utility = ["/usr/bin/xdg-mime", "/bin/xdg-mime"]
        .into_iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .ok_or(Error::Unsupported)?;
    register_at(exe, &data, |set| invoke(utility, set))
}

// Separate environment/command boundary lets tests exercise the production
// filesystem and ownership flow without touching the host's associations.
fn register_at(
    exe: &Path,
    data: &Path,
    mut run: impl FnMut(bool) -> Result<String, Error>,
) -> Result<(), Error> {
    let contents = entry(exe)?;
    let check = |value: String| {
        if value.trim().is_empty() || value.trim() == DESKTOP {
            Ok(())
        } else {
            Err(Error::Conflict)
        }
    };
    let initial = run(false)?;
    check(initial.clone())?;
    let dir = data.join("applications");
    let path = dir.join(DESKTOP);
    // A same-named system entry is not this installation. Never shadow it
    // merely because xdg-mime reports our desktop ID as the effective default.
    if initial.trim() == DESKTOP {
        validate_entry(&path, &contents)?;
    }
    fs::create_dir_all(&dir).map_err(|_| Error::Failed)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(contents.as_bytes())
                .map_err(|_| Error::Failed)?;
            file.sync_all().map_err(|_| Error::Failed)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            validate_entry(&path, &contents)?;
        }
        Err(_) => return Err(Error::Failed),
    }
    // Another app may have installed an association while our file was made.
    // No OS-wide compare-and-swap exists; refuse any conflict we can observe.
    let current = run(false)?;
    check(current.clone())?;
    if current.trim() != DESKTOP {
        run(true)?;
        if run(false)?.trim() != DESKTOP {
            return Err(Error::Failed);
        }
    }
    validate_entry(&path, &contents)
}

fn validate_entry(path: &Path, contents: &str) -> Result<(), Error> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| Error::Conflict)?;
    if !file.metadata().map_err(|_| Error::Conflict)?.is_file() {
        return Err(Error::Conflict);
    }
    let mut saved = String::new();
    Read::by_ref(&mut file)
        .take(LIMIT as u64 + 1)
        .read_to_string(&mut saved)
        .map_err(|_| Error::Conflict)?;
    if saved != contents {
        return Err(Error::Conflict);
    }
    Ok(())
}

#[allow(unsafe_code)] // nonblocking read of this child's owned stdout fd only
fn invoke(utility: &Path, set: bool) -> Result<String, Error> {
    let mut command = Command::new(utility);
    if set {
        command.args(["default", DESKTOP, MIME]);
    } else {
        command.args(["query", "default", MIME]);
    }
    command.process_group(0);
    let mut child = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|_| Error::Failed)?;
    let group = child.id();
    let Some(mut stdout) = child.stdout.take() else {
        kill_group(group);
        let _ = child.wait();
        return Err(Error::Failed);
    };
    // SAFETY: fd belongs to the live ChildStdout; only its nonblocking flag is set.
    if unsafe { libc::fcntl(stdout.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
        kill_group(group);
        let _ = child.wait();
        return Err(Error::Failed);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut bytes = Vec::new();
    let mut ended = None;
    let mut owned = true;
    let result = loop {
        if Instant::now() >= deadline {
            break Err(Error::Failed);
        }
        if ended.is_none() {
            match observe_exit(group) {
                Ok(status) => ended = status,
                Err(error) => {
                    owned = error.raw_os_error() != Some(libc::ECHILD);
                    break Err(Error::Failed);
                }
            }
        }
        let mut chunk = [0; 512];
        let mut eof = false;
        match stdout.read(&mut chunk) {
            Ok(n) => {
                if bytes.len() + n > LIMIT {
                    break Err(Error::Failed);
                }
                bytes.extend_from_slice(&chunk[..n]);
                if n > 0 {
                    continue;
                }
                eof = true;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break Err(Error::Failed),
        }
        if let Some(status) = ended {
            // xdg-mime completes its helpers synchronously. A surviving
            // stdout owner means unexpected asynchronous work, not success.
            break if status && eof {
                String::from_utf8(bytes).map_err(|_| Error::Failed)
            } else {
                Err(Error::Failed)
            };
        }
        if Instant::now() >= deadline {
            break Err(Error::Failed);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    // Also bounded when a grandchild keeps stdout open; never wait for EOF.
    if owned {
        kill_group(group);
    }
    let _ = child.wait();
    result
}

#[allow(unsafe_code)]
fn observe_exit(id: u32) -> std::io::Result<Option<bool>> {
    // SAFETY: zero is a valid siginfo buffer, and waitid writes only that buffer.
    // WNOWAIT leaves the child unreaped, pinning its process/group identity
    // until kill_group has cleaned up and Child::wait finally reaps it.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::waitid(
            libc::P_PID,
            id,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::Interrupted {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: waitid returns child-status siginfo (or a zero pid for no event).
    if unsafe { info.si_pid() } == 0 {
        return Ok(None);
    }
    Ok(Some(
        info.si_code == libc::CLD_EXITED && unsafe { info.si_status() } == 0,
    ))
}

#[allow(unsafe_code)]
fn kill_group(id: u32) {
    // SAFETY: process_group(0) creates a new group owned by this child; negative
    // pid addresses only that group, including xdg-mime's helper descendants.
    let _ = unsafe { libc::kill(-(id as libc::pid_t), libc::SIGKILL) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "fastcord-links-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn registration_is_explicit_owned_and_idempotent_without_other_schemes() {
        let dir = Dir::new();
        let exe = Path::new("/opt/Fast Cord/fastcord");
        let mut default = String::new();
        let mut sets = 0;
        for _ in 0..2 {
            register_at(exe, &dir.0, |set| {
                if set {
                    default = DESKTOP.into();
                    sets += 1;
                }
                Ok(default.clone())
            })
            .unwrap();
        }
        assert_eq!(sets, 1);
        let saved = fs::read_to_string(dir.0.join("applications").join(DESKTOP)).unwrap();
        assert!(saved.contains("Exec=\"/opt/Fast Cord/fastcord\" --open-url %u\n"));
        assert!(!saved.contains("discord://"));
        assert_eq!(fs::read_dir(dir.0.join("applications")).unwrap().count(), 1);
    }

    #[test]
    fn foreign_default_or_entry_and_racing_association_are_preserved() {
        let dir = Dir::new();
        let exe = Path::new("/opt/fastcord");
        assert_eq!(
            register_at(exe, &dir.0, |_| Ok("other.desktop".into())),
            Err(Error::Conflict)
        );
        assert!(!dir.0.join("applications").exists());
        fs::create_dir_all(dir.0.join("applications")).unwrap();
        let path = dir.0.join("applications").join(DESKTOP);
        fs::write(&path, "foreign").unwrap();
        assert_eq!(
            register_at(exe, &dir.0, |set| {
                assert!(!set);
                Ok(String::new())
            }),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "foreign");
        fs::remove_file(&path).unwrap();
        let mut reads = 0;
        assert_eq!(
            register_at(exe, &dir.0, |set| {
                assert!(!set);
                reads += 1;
                Ok(if reads == 1 {
                    String::new()
                } else {
                    "other.desktop".into()
                })
            }),
            Err(Error::Conflict)
        );
    }

    #[test]
    fn symlink_and_changed_executable_are_not_overwritten() {
        let dir = Dir::new();
        fs::create_dir_all(dir.0.join("applications")).unwrap();
        let target = dir.0.join("foreign");
        fs::write(&target, "keep").unwrap();
        let path = dir.0.join("applications").join(DESKTOP);
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert_eq!(
            register_at(Path::new("/opt/fastcord"), &dir.0, |_| Ok(String::new())),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "keep");
        let path = dir.0.join("applications").join(DESKTOP);
        fs::remove_file(&path).unwrap();
        let old = entry(Path::new("/opt/old-fastcord")).unwrap();
        fs::write(&path, &old).unwrap();
        assert_eq!(
            register_at(Path::new("/opt/new-fastcord"), &dir.0, |set| {
                assert!(!set);
                Ok(DESKTOP.into())
            }),
            Err(Error::Conflict)
        );
        assert_eq!(fs::read_to_string(path).unwrap(), old);
    }

    #[test]
    fn command_output_and_runtime_are_bounded() {
        assert_eq!(invoke(Path::new("/bin/false"), false), Err(Error::Failed));
        // /usr/bin/yes ignores these operands and floods stdout; the cap stops it.
        assert_eq!(invoke(Path::new("/usr/bin/yes"), false), Err(Error::Failed));
    }
    #[test]
    #[allow(unsafe_code)]
    fn fifo_entry_is_refused_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        const CHILD: &str = "FASTCORD_TEST_LINK_FIFO_ROOT";
        let Some(root) = std::env::var_os(CHILD) else {
            let dir = Dir::new();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "link_handler::linux::tests::fifo_entry_is_refused_without_waiting_for_a_writer", "--nocapture"])
                .env(CHILD, &dir.0).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::inherit())
                .spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        assert!(status.success());
                        assert!(
                            dir.0.join("ran").is_file(),
                            "exact child fixture must execute"
                        );
                        return;
                    }
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("FIFO ownership check blocked");
                    }
                }
            }
        };
        let dir = PathBuf::from(root);
        fs::create_dir_all(dir.join("applications")).unwrap();
        let path = dir.join("applications").join(DESKTOP);
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: terminated temporary path, no host registration location.
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        assert_eq!(
            register_at(Path::new("/opt/fastcord"), &dir, |set| {
                assert!(!set);
                Ok(String::new())
            }),
            Err(Error::Conflict)
        );
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_fifo());
        fs::write(dir.join("ran"), b"ok").unwrap();
    }

    #[test]
    fn helper_descendants_cannot_write_after_the_command_has_returned() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Dir::new();
        let utility = dir.0.join("utility");
        let marker = dir.0.join("late-write");
        // Immediate parent exits successfully with a background descendant.
        // The process-group owner must terminate that descendant too.
        fs::write(
            &utility,
            format!(
                "#!/bin/sh\n(sleep 1; echo late > '{}') &\nexit 0\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&utility, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(invoke(&utility, false), Err(Error::Failed));
        std::thread::sleep(Duration::from_millis(1200));
        assert!(!marker.exists());
    }

    #[test]
    fn same_id_in_another_xdg_directory_is_not_shadowed() {
        let dir = Dir::new();
        assert_eq!(
            register_at(Path::new("/opt/fastcord"), &dir.0, |set| {
                assert!(!set);
                Ok(DESKTOP.into())
            }),
            Err(Error::Conflict)
        );
        assert!(!dir.0.join("applications").exists());
    }
}
