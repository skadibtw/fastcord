//! Actual executable launch/handoff checks. Never registers a URL scheme or
//! signs in. Valid links use a private Linux primary; malformed envelopes must
//! return before any UI/account initialization on every platform.
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
struct Sandbox(std::path::PathBuf);
impl Sandbox {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fc-url-data-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }
}
impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn bounded_wait(mut child: Child, timeout: Duration) -> std::process::ExitStatus {
    let until = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status,
            Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(10)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned URL-launch child did not exit: {result:?}");
            }
        }
    }
}
fn launch(args: &[&str]) {
    let sandbox = Sandbox::new();
    let mut command = Command::new(env!("CARGO_BIN_EXE_fastcord"));
    command.args(args);
    for name in [
        "HOME",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
        "XDG_DATA_HOME",
    ] {
        command.env(name, &sandbox.0);
    }
    command.env(
        "DBUS_SESSION_BUS_ADDRESS",
        format!("unix:path={}/no-session-bus", sandbox.0.display()),
    );
    if std::env::var_os("FASTCORD_TEST_PRIVATE_URL_PRIMARY").is_none() {
        command.env("XDG_RUNTIME_DIR", &sandbox.0);
    }
    let child = command
        // If envelope validation regresses, the only alternative tested below
        // is the static Alerts preview (no session, subscription or REST client).
        .env("DISPLAY", "")
        .env("WAYLAND_DISPLAY", "")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    assert!(bounded_wait(child, Duration::from_secs(15)).success());
}

#[test]
fn registered_url_launch_rejects_extra_argv_before_offline_preview_dispatch() {
    launch(&[
        "--open-url",
        "fastcord://-/channels/1/2",
        "--alerts-preview",
    ]);
    launch(&[
        "--open-url",
        "fastcord://-/channels/1/2\" --alerts-preview",
        "--alerts-preview",
    ]);
}

#[cfg(target_os = "linux")]
#[test]
fn registered_url_launch_hands_navigation_to_private_primary_once() {
    use fastcord_platform::instance::{self, Forwarded, Start};
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = "FASTCORD_TEST_PRIVATE_URL_PRIMARY";
    if std::env::var_os(CHILD).is_some() {
        let (send, receive) = std::sync::mpsc::sync_channel(8);
        let Start::Primary(_guard) = instance::acquire(None, move |event| {
            let _ = send.try_send(event);
        }) else {
            panic!("private test primary could not start");
        };
        launch(&["--open-url", "fastcord://-/channels/5001/6005"]);
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(3)).unwrap(),
            Forwarded::Open("fastcord://-/channels/5001/6005".into())
        );
        launch(&["--open-url", "fastcord://-/invite/fakeinvite"]);
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(3)).unwrap(),
            Forwarded::Open("fastcord://-/invite/fakeinvite".into())
        );
        launch(&[
            "--open-url",
            "fastcord://-/ack/5001/6005",
            "--alerts-preview",
        ]);
        assert!(matches!(
            receive.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        return;
    }
    // Environment is set only on the subprocess; parallel tests cannot borrow
    // this private namespace and no process-global set_var is necessary.
    let path = std::env::temp_dir().join(format!("fc-url-{}", std::process::id()));
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "registered_url_launch_hands_navigation_to_private_primary_once",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env("XDG_RUNTIME_DIR", &path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let result = std::panic::catch_unwind(|| bounded_wait(child, Duration::from_secs(60)));
    let _ = std::fs::remove_dir_all(path);
    assert!(result.unwrap().success());
}
