//! The test-only fake mode: `fastcord --fake-discord <port>` talks to the
//! loopback `fastcord-fake` server instead of Discord (debug builds only).
//!
//! It signs in at once with [`TOKEN`], memory-only, so no account, credential
//! store, tray, autostart or single-instance hand-off is involved, and it keeps
//! its settings and caches under `fastcord-fake` (see `audio_devices::app_dir_name`).
//!
//! `--script <file>` runs a UI script for layout checks without a person at
//! the screen: one step per line, `wait <seconds>`, `open <link>` (as a second
//! launch would), `size <width> <height>`, `toggle <panel>` (open or close one
//! of [`PANELS`] as its button would), `join <guild> <channel>` (join a voice
//! channel; the fake answers no voice, so it stays connecting), `snap <name>` (the window's own pixels
//! to `<name>.png` beside the script; the player window's while one is open),
//! `stream <seconds>` (a synthetic watched stream, see [`Step::Stream`]) and `quit`.

use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;

use iced::widget::{container, text};
use iced::{Element, Length};

/// The only token the fake server accepts.
pub const TOKEN: &str = "fake-token";

static PORT: OnceLock<u16> = OnceLock::new();
static SCRIPT: OnceLock<PathBuf> = OnceLock::new();
static DRAW_SHELL: OnceLock<iced_renderer::graphics::Shell> = OnceLock::new();
static SCRIPT_FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn script_failed() -> bool {
    SCRIPT_FAILED.load(std::sync::atomic::Ordering::Relaxed)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SnapshotError {
    CaptureCanceled,
    DrawTimeout,
    Oversized,
    Presentation,
    CompletionCanceled,
    CompletionTimeout,
    Save(String),
}

impl std::fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CaptureCanceled => f.write_str("capture canceled"),
            Self::DrawTimeout => f.write_str("window did not present before the deadline"),
            Self::Oversized => f.write_str("capture exceeds the pixel budget"),
            Self::Presentation => f.write_str("window presentation failed"),
            Self::CompletionCanceled => f.write_str("screenshot completion canceled"),
            Self::CompletionTimeout => f.write_str("screenshot completion timed out"),
            Self::Save(error) => write!(f, "PNG save failed: {error}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

type CaptureResult = Result<iced::window::Screenshot, SnapshotError>;
type SnapshotResult = Result<(), SnapshotError>;

pub(crate) fn set_draw_shell(shell: iced_renderer::graphics::Shell) {
    let _ = DRAW_SHELL.set(shell);
}

// At most one scripted screenshot may await a completed draw. A newer
// request cancels the old one; dropping/timing out its ticket releases it.
#[derive(Default)]
struct DrawWaiter {
    serial: u64,
    pending: Option<(
        u64,
        iced::window::Id,
        tokio::sync::oneshot::Sender<CaptureResult>,
    )>,
}

static DRAW_WAITER: std::sync::Mutex<DrawWaiter> = std::sync::Mutex::new(DrawWaiter {
    serial: 0,
    pending: None,
});

pub(crate) struct DrawTicket {
    serial: u64,
    ready: tokio::sync::oneshot::Receiver<CaptureResult>,
}

pub(crate) fn next_draw(window: iced::window::Id) -> DrawTicket {
    let (send, ready) = tokio::sync::oneshot::channel();
    let mut waiter = DRAW_WAITER.lock().unwrap_or_else(|p| p.into_inner());
    waiter.serial = waiter.serial.wrapping_add(1);
    let serial = waiter.serial;
    waiter.pending = Some((serial, window, send));
    drop(waiter);
    // A script barrier deliberately prevents the next UI action. Wake the
    // runtime explicitly instead of relying on that action to produce a frame.
    // iced's shell API redraws all windows; take_capture still selects only
    // the requested window. No timer or screenshot action is scheduled.
    if let Some(shell) = DRAW_SHELL.get() {
        shell.request_redraw();
    }
    DrawTicket { serial, ready }
}

/// Takes a request only at the matching window's presentation boundary.
/// The caller captures and sends owned pixels synchronously, while its widget
/// paragraphs are alive; it must not schedule a later screenshot action.
pub(crate) fn take_capture(
    window: iced::window::Id,
) -> Option<tokio::sync::oneshot::Sender<CaptureResult>> {
    let mut waiter = DRAW_WAITER.lock().unwrap_or_else(|p| p.into_inner());
    if waiter
        .pending
        .as_ref()
        .is_some_and(|(_, wanted, _)| *wanted == window)
    {
        waiter.pending.take().map(|(_, _, ready)| ready)
    } else {
        None
    }
}

/// Single pending RGBA snapshot budget, including high-DPI windows.
pub(crate) fn capture_size_allowed(size: iced::Size<u32>) -> bool {
    size.width > 0
        && size.height > 0
        && u64::from(size.width) * u64::from(size.height) <= 16 * 1024 * 1024
}

impl DrawTicket {
    pub(crate) async fn wait(self) -> CaptureResult {
        self.wait_for(Duration::from_secs(3)).await
    }

    async fn wait_for(mut self, timeout: Duration) -> CaptureResult {
        tokio::time::timeout(timeout, &mut self.ready)
            .await
            .map_err(|_| SnapshotError::DrawTimeout)?
            .map_err(|_| SnapshotError::CaptureCanceled)?
    }
}

impl Drop for DrawTicket {
    fn drop(&mut self) {
        let mut waiter = DRAW_WAITER.lock().unwrap_or_else(|p| p.into_inner());
        if waiter
            .pending
            .as_ref()
            .is_some_and(|(serial, _, _)| *serial == self.serial)
        {
            waiter.pending = None;
        }
    }
}

/// Request-owned completion: clones refer to the same capture, never to a
/// later request using the same path. Dropping the last clone cancels the wait.
#[derive(Clone, Debug)]
pub struct SnapshotAck(
    std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<SnapshotResult>>>>,
);

impl PartialEq for SnapshotAck {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl SnapshotAck {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<SnapshotResult>) {
        let (send, ready) = tokio::sync::oneshot::channel();
        (
            Self(std::sync::Arc::new(std::sync::Mutex::new(Some(send)))),
            ready,
        )
    }

    pub(crate) fn finish(&self, result: SnapshotResult) {
        if let Some(send) = self.0.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = send.send(result);
        }
    }
}

/// A snap is a barrier through PNG saving, not merely through sending the
/// app message. If the app stops processing messages, stop this script after
/// the bounded wait rather than emitting actions against an unknown frame.
async fn send_step(
    output: &mut iced::futures::channel::mpsc::Sender<Step>,
    step: Step,
    timeout: Duration,
) -> bool {
    use iced::futures::SinkExt;
    let (step, ready) = match step {
        Step::Snap(path, _) => {
            let (ack, ready) = SnapshotAck::new();
            (Step::Snap(path.clone(), Some(ack)), Some((path, ready)))
        }
        step => (step, None),
    };
    if output.send(step).await.is_err() {
        return false;
    }
    if let Some((path, ready)) = ready {
        let result = match tokio::time::timeout(timeout, ready).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(SnapshotError::CompletionCanceled),
            Err(_) => Err(SnapshotError::CompletionTimeout),
        };
        if let Err(error) = result {
            SCRIPT_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!("script: result=failed screenshot={path:?} reason={error:?}");
            let _ = output.send(Step::Quit).await;
            return false;
        }
    }

    true
}

/// Panels a script can open or close.
pub const PANELS: &[&str] = &[
    "search",
    "discover",
    "discover-filter",
    "discover-no-match",
    "discover-clear",
    "pins",
    "thread-archive",
    "archive-public",
    "archive-private",
    "archive-joined",
    "spelling-prepare",
    "spelling-en-us",
    "spelling-apply",
    "inbox",
    "friends",
    "emoji",
    "audio",
    "app",
    "links-register",
    "links-conflict",
    "links-pending",
    "network",
    "chat",
    "account",
    "login-qr",
    "login-password",
    "login-token",
    "login-accounts",
    "server-menu",
    "browse-channels",
    "events",
    "event-details",
    "event-users",
    "event-users-next",
    "event-new",
    "event-weekly",
    "event-edit",
    "event-once",
    "event-keep-repeat",
    "community-list-open",
    "community-list-onboarding",
    "community-list-question",
    "community-list-answer",
    "community-list-end",
    "onboarding-questions",
    "community-setup",
    "community-onboarding",
    "community-question",
    "community-answer",
    "community-custom",
    "community-clear",
    "community-discard",
    "community-emoji-scroll",
    "conversation-menu",
    "account-menu",
    "mic-menu",
    "new-dm",
    "requests",
    "settings",
    "close-menus",
    "escape",
    "gif",
    "sticker",
    "profile",
    "accounts",
    "server-settings",
    "roles",
    "bans",
    "automod",
    "group",
    "long-draft",
    "create-channel",
    "network-details",
    "friend-menu",
];

/// One step of a `--script`.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Open(String),
    /// Type a query or activate a scoped slash-command form control.
    Slash(String),
    DmSort(fastcord_discord::state::navigation::PrivateSort),
    /// Exercise a scoped local entity selector.
    Entity(String),
    ComponentText(String),
    Size(f32, f32),
    /// One of [`PANELS`].
    Toggle(&'static str),
    /// A voice channel of a server.
    Join(u64, u64),
    /// Where the window's pixels go (the player window's while one is open).
    Snap(PathBuf, Option<SnapshotAck>),
    /// Watch a synthetic stream for this many seconds: a player window opens
    /// and 1280x720 pictures arrive at 30 fps (no voice exists offline).
    Stream(f32),
    Quit,
}

/// A bounded pause also keeps Tokio's platform-specific Instant arithmetic safe.
const MAX_SCRIPT_PAUSE: Duration = Duration::from_secs(600);

#[derive(Debug, PartialEq, Eq)]
struct ScriptWaitError {
    line: usize,
}

impl std::fmt::Display for ScriptWaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "line {}: wait must be finite and nonnegative; combined pause must not exceed 600 seconds",
            self.line
        )
    }
}

impl std::error::Error for ScriptWaitError {}

/// Validate every pause before emitting any action. Other bad commands retain
/// their existing stderr diagnostic and are skipped.
fn parse_script(
    text: &str,
    dir: &std::path::Path,
) -> Result<Vec<(Duration, Step)>, ScriptWaitError> {
    let mut steps = Vec::new();
    let mut pause = Duration::ZERO;
    for (index, line) in text
        .lines()
        .enumerate()
        .map(|(index, line)| (index, line.trim()))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
    {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let step = match verb {
            "wait" => {
                pause = rest
                    .parse::<f32>()
                    .ok()
                    .and_then(|seconds| Duration::try_from_secs_f32(seconds).ok())
                    .and_then(|wait| pause.checked_add(wait))
                    .filter(|pause| *pause <= MAX_SCRIPT_PAUSE)
                    .ok_or(ScriptWaitError { line: index + 1 })?;
                continue;
            }
            "open" => Step::Open(rest.to_owned()),
            "slash" => Step::Slash(rest.to_owned()),
            "sort-dms" => match rest {
                "recent" => Step::DmSort(fastcord_discord::state::navigation::PrivateSort::Recent),
                "unread-first" => {
                    Step::DmSort(fastcord_discord::state::navigation::PrivateSort::UnreadFirst)
                }
                _ => {
                    eprintln!("script: bad DM sort: {line}");
                    continue;
                }
            },
            "entity" if rest.len() <= 512 => Step::Entity(rest.to_owned()),
            "component-text" if rest.len() <= 128 => Step::ComponentText(rest.to_owned()),
            "size" => match rest
                .split_once(' ')
                .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            {
                Some((width, height)) => Step::Size(width, height),
                None => {
                    eprintln!("script: bad size: {line}");
                    continue;
                }
            },
            "snap" if !rest.is_empty() && !rest.contains(['/', '\\']) => {
                Step::Snap(dir.join(format!("{rest}.png")), None)
            }
            "toggle" => match PANELS.iter().find(|panel| **panel == rest) {
                Some(panel) => Step::Toggle(panel),
                None => {
                    eprintln!("script: unknown panel: {line}");
                    continue;
                }
            },
            "join" => match rest
                .split_once(' ')
                .and_then(|(g, c)| Some((g.trim().parse().ok()?, c.trim().parse().ok()?)))
            {
                Some((guild, channel)) => Step::Join(guild, channel),
                None => {
                    eprintln!("script: bad join: {line}");
                    continue;
                }
            },
            "stream" => match rest.parse::<f32>() {
                Ok(seconds) if (0.0..=600.0).contains(&seconds) => Step::Stream(seconds),
                _ => {
                    eprintln!("script: bad stream: {line}");
                    continue;
                }
            },
            "quit" => Step::Quit,
            _ => {
                eprintln!("script: unknown step: {line}");
                continue;
            }
        };
        steps.push((std::mem::take(&mut pause), step));
    }
    Ok(steps)
}

async fn run_script(
    output: &mut iced::futures::channel::mpsc::Sender<Step>,
    text: &str,
    dir: &std::path::Path,
) -> bool {
    use iced::futures::SinkExt;
    let steps = match parse_script(text, dir) {
        Ok(steps) => steps,
        Err(error) => {
            SCRIPT_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
            eprintln!("script: result=failed stage=parse reason={error}");
            let _ = output.send(Step::Quit).await;
            return false;
        }
    };
    for (pause, step) in steps {
        tokio::time::sleep(pause).await;
        if !send_step(output, step, Duration::from_secs(10)).await {
            return false;
        }
    }
    true
}

/// The steps of `--script`, each after its pause; nothing without one.
pub fn script() -> impl iced::futures::Stream<Item = Step> {
    script_for(active(), SCRIPT.get().cloned())
}

fn script_for(enabled: bool, path: Option<PathBuf>) -> impl iced::futures::Stream<Item = Step> {
    iced::stream::channel(1, async move |mut output| {
        // Defense in depth: no script I/O or actions outside the loopback mode.
        if !enabled {
            std::future::pending::<()>().await;
            return;
        }
        let Some(path) = path else {
            std::future::pending::<()>().await;
            return;
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                use iced::futures::SinkExt;
                SCRIPT_FAILED.store(true, std::sync::atomic::Ordering::Relaxed);
                eprintln!("script: result=failed stage=read path={path:?} reason={error}");
                let _ = output.send(Step::Quit).await;
                return;
            }
        };
        let dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_owned();
        if !run_script(&mut output, &text, &dir).await {
            return;
        }
        std::future::pending::<()>().await;
    })
}

/// Writes a window screenshot as an RGBA PNG.
pub(crate) fn save(path: &std::path::Path, shot: &iced::window::Screenshot) -> SnapshotResult {
    image::save_buffer(
        path,
        &shot.rgba,
        shot.size.width,
        shot.size.height,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|error| SnapshotError::Save(error.to_string()))?;
    eprintln!("script: saved {}", path.display());
    Ok(())
}

pub fn active() -> bool {
    PORT.get().is_some()
}

/// `--login-screen`: start on the sign-in screen instead of signing in.
pub fn login_screen() -> bool {
    std::env::args().any(|arg| arg == "--login-screen")
}

#[derive(Debug, Default, PartialEq, Eq)]
struct FakeArgs {
    port: Option<u16>,
    script: Option<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
enum FakeArgsError {
    MissingScript,
    InvalidPort,
    Duplicate(&'static str),
    ScriptNeedsFake,
}

impl std::fmt::Display for FakeArgsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingScript => f.write_str("--script needs a file path"),
            Self::InvalidPort => f.write_str("--fake-discord needs a bare nonzero port number, for example --fake-discord 8765"),
            Self::Duplicate(flag) => write!(f, "{flag} may only be supplied once"),
            Self::ScriptNeedsFake => f.write_str("--script requires --fake-discord <port>; scripts cannot run against a real account"),
        }
    }
}

impl std::error::Error for FakeArgsError {}

fn parse_args(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<FakeArgs, FakeArgsError> {
    let mut args = args.into_iter();
    let mut parsed = FakeArgs::default();
    while let Some(arg) = args.next() {
        if arg == "--script" {
            if parsed.script.is_some() {
                return Err(FakeArgsError::Duplicate("--script"));
            }
            let path = args
                .next()
                .filter(|value| !value.as_encoded_bytes().starts_with(b"--"))
                .ok_or(FakeArgsError::MissingScript)?;
            if path.is_empty() {
                return Err(FakeArgsError::MissingScript);
            }
            parsed.script = Some(PathBuf::from(path));
        } else if arg == "--fake-discord" {
            if parsed.port.is_some() {
                return Err(FakeArgsError::Duplicate("--fake-discord"));
            }
            parsed.port = Some(
                args.next()
                    .as_deref()
                    .and_then(std::ffi::OsStr::to_str)
                    .and_then(parse_port)
                    .ok_or(FakeArgsError::InvalidPort)?,
            );
        }
    }
    if parsed.script.is_some() && parsed.port.is_none() {
        return Err(FakeArgsError::ScriptNeedsFake);
    }
    Ok(parsed)
}

fn validate_script_file(path: &std::path::Path) -> std::io::Result<()> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "script must be a regular file",
        ));
    }
    std::fs::File::open(path)?;
    Ok(())
}

fn invalid_args(error: impl std::fmt::Display) -> ! {
    eprintln!("fake arguments: {error}");
    std::process::exit(2);
}

/// Validate the whole argument set before any GUI, account restoration or
/// credential access. Script/port order is immaterial; flags alone never
/// enable script execution without the successfully installed loopback origin.
pub fn init() {
    let parsed =
        parse_args(std::env::args_os().skip(1)).unwrap_or_else(|error| invalid_args(error));
    if let Some(path) = &parsed.script
        && let Err(error) = validate_script_file(path)
    {
        invalid_args(format!("cannot read script {}: {error}", path.display()));
    }
    if let Some(port) = parsed.port {
        if fastcord_discord::fake_origin::set_origin(&format!("127.0.0.1:{port}")).is_err() {
            invalid_args("--fake-discord: the origin could not be set");
        }
        let _ = PORT.set(port);
    }
    if let Some(path) = parsed.script {
        let _ = SCRIPT.set(path);
    }
}

fn parse_port(text: &str) -> Option<u16> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|port| *port != 0)
}

/// The thin strip across the top of the window.
pub fn banner<'a, Message: 'a>() -> Option<Element<'a, Message>> {
    let port = *PORT.get()?;
    Some(
        container(
            text(format!(
                "Fake Discord on 127.0.0.1:{port}: nothing reaches discord.com"
            ))
            .size(12),
        )
        .width(Length::Fill)
        .padding([2, 8])
        .style(|theme: &iced::Theme| {
            let pair = theme.extended_palette().danger.strong;
            container::Style {
                background: Some(pair.color.into()),
                text_color: Some(pair.text),
                ..container::Style::default()
            }
        })
        .into(),
    )
}

/// Feeds a synthetic 1280x720 NV12 picture at 30 fps into `session` for
/// `seconds` on a thread of its own, the way the stream decoder's sink does.
pub fn feed(session: std::sync::Arc<crate::playback::Session>, seconds: f32) {
    use fastcord_video::DecodedFrame;
    use fastcord_video::frame::{ColorDescription, Nv12};
    std::thread::spawn(move || {
        let (width, height) = (1280usize, 720usize);
        let mut sink = session.sink();
        let mut picture = vec![128u8; width * height * 3 / 2];
        let started = std::time::Instant::now();
        let mut n = 0u32;
        while started.elapsed().as_secs_f32() < seconds
            && session.active.load(std::sync::atomic::Ordering::Acquire)
        {
            for row in 0..height {
                for x in 0..width {
                    picture[row * width + x] = ((x as u32 + n * 6) / 5 % 256) as u8;
                }
            }
            for pair in picture[width * height..].as_chunks_mut::<2>().0 {
                pair[0] = (n * 3 % 256) as u8;
                pair[1] = 255 - (n * 2 % 256) as u8;
            }
            if let Ok(image) = Nv12::packed(width as u32, height as u32, &picture) {
                sink(
                    0,
                    DecodedFrame {
                        image,
                        color: ColorDescription::default(),
                        timestamp: started.elapsed(),
                    },
                );
            }
            n += 1;
            std::thread::sleep(std::time::Duration::from_millis(33));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn component_text_script_step_is_bounded() {
        let steps = parse_script("component-text reveal 10 0 0\n", std::path::Path::new("."));
        assert!(matches!(&steps[0].1,Step::ComponentText(command) if command=="reveal 10 0 0"));
        let too_long = format!("component-text {}", "x".repeat(129));
        assert!(parse_script(&too_long, std::path::Path::new(".")).is_empty());
    }

    #[test]
    fn scripts_parse_steps_with_their_pauses() {
        let dir = std::path::Path::new("shots");
        let steps = parse_script(
            "# comment\nwait 2\nopen discord://-/channels/@me\nsize 1080 672\nwait 0.5\nwait 0.5\nsnap home\nsnap ../x\nbogus\ntoggle pins\ntoggle nothing\njoin 5001 6009\nquit\n",
            dir,
        ).unwrap();
        assert_eq!(
            steps,
            [
                (
                    Duration::from_secs(2),
                    Step::Open("discord://-/channels/@me".into())
                ),
                (Duration::ZERO, Step::Size(1080.0, 672.0)),
                (
                    Duration::from_secs(1),
                    Step::Snap(dir.join("home.png"), None)
                ),
                (Duration::ZERO, Step::Toggle("pins")),
                (Duration::ZERO, Step::Join(5001, 6009)),
                (Duration::ZERO, Step::Quit),
            ]
        );
    }

    #[test]
    fn waits_reject_nonfinite_overflow_and_excessive_accumulation() {
        let dir = std::path::Path::new("shots");
        for wait in [
            "inf",
            "-inf",
            "NaN",
            "1e30",
            "18446744073709551616",
            "-1",
            "601",
            "",
        ] {
            let text = format!("# line 1\nwait {wait}\nsnap after\nquit");
            assert_eq!(
                parse_script(&text, dir),
                Err(ScriptWaitError { line: 2 }),
                "{wait}"
            );
        }
        assert_eq!(
            parse_script("wait 400\nwait 201\nquit", dir),
            Err(ScriptWaitError { line: 2 })
        );
        assert_eq!(
            parse_script(
                "wait 0\nwait 0.25\nwait 0.75\ntoggle pins\nwait 600\nquit",
                dir
            )
            .unwrap(),
            vec![
                (Duration::from_secs(1), Step::Toggle("pins")),
                (MAX_SCRIPT_PAUSE, Step::Quit)
            ]
        );
    }

    #[tokio::test]
    async fn invalid_wait_fails_before_emitting_any_script_action() {
        use iced::futures::StreamExt;
        for text in [
            "toggle pins\nwait inf\nquit",
            "wait 400\nwait 201\ntoggle pins\nquit",
        ] {
            let (mut output, mut input) = iced::futures::channel::mpsc::channel(1);
            let driver = tokio::spawn(async move {
                run_script(&mut output, text, std::path::Path::new(".")).await
            });
            assert_eq!(input.next().await, Some(Step::Quit));
            assert!(!driver.await.unwrap());
            assert!(input.next().await.is_none());
            assert!(script_failed());
        }
    }

    fn args(values: &[&str]) -> Result<FakeArgs, FakeArgsError> {
        parse_args(values.iter().map(std::ffi::OsString::from))
    }

    #[test]
    fn scripts_require_valid_loopback_arguments_in_either_order() {
        let expected = FakeArgs {
            port: Some(8765),
            script: Some(PathBuf::from("checks.txt")),
        };
        assert_eq!(
            args(&["--script", "checks.txt", "--fake-discord", "8765"]),
            Ok(expected)
        );
        assert_eq!(
            args(&[
                "--fake-discord",
                "8765",
                "--login-screen",
                "--script",
                "checks.txt"
            ]),
            Ok(FakeArgs {
                port: Some(8765),
                script: Some(PathBuf::from("checks.txt"))
            })
        );
        assert_eq!(args(&[]), Ok(FakeArgs::default()));
        assert_eq!(args(&["discord://-/channels/@me"]), Ok(FakeArgs::default()));
        assert_eq!(
            args(&["--fake-discord", "8765"]),
            Ok(FakeArgs {
                port: Some(8765),
                script: None
            })
        );
        assert_eq!(
            args(&["--script", "checks.txt"]),
            Err(FakeArgsError::ScriptNeedsFake)
        );
        for invalid in [
            vec!["--script"],
            vec!["--script", ""],
            vec!["--script", "--fake-discord", "8765"],
        ] {
            assert_eq!(args(&invalid), Err(FakeArgsError::MissingScript));
        }
        for invalid in [
            vec!["--fake-discord"],
            vec!["--fake-discord", "--script", "checks.txt"],
            vec!["--fake-discord", "0"],
            vec!["--fake-discord", "discord.com:443"],
            vec!["--fake-discord", "65536"],
        ] {
            assert_eq!(args(&invalid), Err(FakeArgsError::InvalidPort));
        }
        assert_eq!(
            args(&["--fake-discord", "8765", "--fake-discord", "8765"]),
            Err(FakeArgsError::Duplicate("--fake-discord"))
        );
        assert_eq!(
            args(&["--script", "a", "--script", "b", "--fake-discord", "8765"]),
            Err(FakeArgsError::Duplicate("--script"))
        );
    }

    #[tokio::test]
    async fn inactive_script_stream_never_reads_or_emits_actions() {
        use iced::futures::{FutureExt, StreamExt};
        let missing = std::env::temp_dir().join(format!(
            "fastcord-missing-script-{}.txt",
            std::process::id()
        ));
        assert!(!missing.exists());
        assert!(validate_script_file(&missing).is_err());
        assert!(validate_script_file(&std::env::temp_dir()).is_err());
        let inactive = script_for(false, Some(missing.clone()));
        iced::futures::pin_mut!(inactive);
        assert!(
            inactive.next().now_or_never().is_none(),
            "inactive stream must not even report a read failure"
        );
        let enabled = script_for(true, Some(missing));
        iced::futures::pin_mut!(enabled);
        assert_eq!(
            enabled.next().await,
            Some(Step::Quit),
            "a file removed after startup fails visibly"
        );
        assert!(script_failed());
    }

    #[tokio::test]
    async fn enabled_script_stream_reads_valid_script() {
        use iced::futures::StreamExt;
        let path = std::env::temp_dir().join(format!(
            "fastcord-enabled-script-{}.txt",
            std::process::id()
        ));
        std::fs::write(&path, "quit\n").unwrap();
        validate_script_file(&path).unwrap();
        let enabled = script_for(true, Some(path.clone()));
        iced::futures::pin_mut!(enabled);
        assert_eq!(enabled.next().await, Some(Step::Quit));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn only_a_bare_nonzero_port_is_accepted() {
        assert_eq!(parse_port("8765"), Some(8765));
        for bad in [
            "",
            "0",
            "-1",
            "+80",
            "65536",
            "127.0.0.1:80",
            "80 ",
            "0x50",
            "discord.com",
        ] {
            assert_eq!(parse_port(bad), None, "{bad}");
        }
    }
}

#[cfg(test)]
mod draw_wait_tests {
    use super::*;

    fn screenshot() -> iced::window::Screenshot {
        iced::window::Screenshot::new(vec![1, 2, 3, 255], iced::Size::new(1, 1), 1.0)
    }

    fn scratch(name: &str) -> PathBuf {
        static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "fastcord-snapshot-{}-{}-{name}",
            std::process::id(),
            SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn script_snapshot_blocks_next_action_until_png_is_saved() {
        use iced::futures::{FutureExt, StreamExt};
        let path = scratch("success.png");
        let target = path.clone();
        let (mut output, mut input) = iced::futures::channel::mpsc::channel(1);
        let driver = tokio::spawn(async move {
            assert!(
                send_step(
                    &mut output,
                    Step::Snap(target, None),
                    Duration::from_secs(10)
                )
                .await
            );
            assert!(send_step(&mut output, Step::Toggle("event-new"), Duration::ZERO).await);
        });
        let Some(Step::Snap(target, Some(ack))) = input.next().await else {
            panic!("request-specific completion");
        };
        assert!(
            input.next().now_or_never().is_none(),
            "next mutation must wait"
        );
        let saved = save(&target, &screenshot());
        assert_eq!(saved, Ok(()));
        assert_eq!(
            image::open(&target).unwrap().into_rgba8().as_raw(),
            &[1, 2, 3, 255]
        );
        ack.finish(saved);
        assert_eq!(input.next().await, Some(Step::Toggle("event-new")));
        driver.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn failed_or_canceled_snapshot_stops_script_and_never_accepts_an_old_png() {
        use iced::futures::StreamExt;
        for failure in [
            None,
            Some(SnapshotError::CaptureCanceled),
            Some(SnapshotError::DrawTimeout),
            Some(SnapshotError::Oversized),
            Some(SnapshotError::Presentation),
        ] {
            let path = scratch("stale.png");
            save(&path, &screenshot()).unwrap();
            let before = std::fs::read(&path).unwrap();
            let target = path.clone();
            let (mut output, mut input) = iced::futures::channel::mpsc::channel(1);
            let driver = tokio::spawn(async move {
                if send_step(
                    &mut output,
                    Step::Snap(target, None),
                    Duration::from_secs(10),
                )
                .await
                {
                    let _ = send_step(&mut output, Step::Toggle("event-new"), Duration::ZERO).await;
                    panic!("failed capture allowed a later mutation");
                }
            });
            let Some(Step::Snap(_, Some(ack))) = input.next().await else {
                panic!("snapshot")
            };
            if let Some(failure) = failure {
                ack.finish(Err(failure));
            } else {
                drop(ack);
            }
            assert_eq!(input.next().await, Some(Step::Quit));
            assert!(input.next().await.is_none());
            driver.await.unwrap();
            assert!(script_failed(), "failed run must exit unsuccessfully");
            assert_eq!(
                std::fs::read(&path).unwrap(),
                before,
                "old PNG is not fresh evidence"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[tokio::test]
    async fn png_save_failure_stops_script_before_following_mutation() {
        use iced::futures::StreamExt;
        let missing = scratch("missing-directory");
        let path = missing.join("failed.png");
        let target = path.clone();
        let (mut output, mut input) = iced::futures::channel::mpsc::channel(1);
        let driver = tokio::spawn(async move {
            if send_step(
                &mut output,
                Step::Snap(target, None),
                Duration::from_secs(10),
            )
            .await
            {
                let _ = send_step(&mut output, Step::Toggle("event-new"), Duration::ZERO).await;
                panic!("failed save allowed a later mutation");
            }
        });
        let Some(Step::Snap(target, Some(ack))) = input.next().await else {
            panic!("snapshot")
        };
        let failed = save(&target, &screenshot());
        assert!(matches!(failed, Err(SnapshotError::Save(_))));
        ack.finish(failed);
        assert_eq!(input.next().await, Some(Step::Quit));
        assert!(input.next().await.is_none());
        driver.await.unwrap();
        assert!(!path.exists());
        assert!(script_failed());
    }

    #[tokio::test]
    async fn script_snapshot_timeout_stops_actions_and_old_ack_cannot_release_new_request() {
        use iced::futures::StreamExt;
        let (mut output, mut input) = iced::futures::channel::mpsc::channel(1);
        let driver = tokio::spawn(async move {
            send_step(
                &mut output,
                Step::Snap(PathBuf::from("same.png"), None),
                Duration::ZERO,
            )
            .await
        });
        let Some(Step::Snap(_, Some(old))) = input.next().await else {
            panic!("snapshot")
        };
        assert!(!driver.await.unwrap());
        let (current, mut ready) = SnapshotAck::new();
        old.finish(Ok(()));
        assert!(matches!(
            ready.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        current.finish(Ok(()));
        assert_eq!(ready.await.unwrap(), Ok(()));
        assert_eq!(input.next().await, Some(Step::Quit));
        assert!(input.next().await.is_none());
    }

    #[tokio::test]
    async fn snapshot_wait_is_single_window_scoped_and_cancellation_safe() {
        struct DrawNotifier(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl iced_renderer::graphics::shell::Notifier for DrawNotifier {
            fn request_redraw(&self) {
                // Registration precedes notification and releases its mutex.
                assert!(DRAW_WAITER.try_lock().unwrap().pending.is_some());
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            fn invalidate_layout(&self) {}
        }
        let draws = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        set_draw_shell(iced_renderer::graphics::Shell::new(DrawNotifier(
            draws.clone(),
        )));
        let first = iced::window::Id::unique();
        let second = iced::window::Id::unique();
        let mut old = next_draw(first);
        assert!(take_capture(second).is_none());
        assert!(matches!(
            old.ready.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let mut current = next_draw(second);
        assert!(matches!(
            old.ready.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        ));
        drop(old); // must not cancel its replacement
        assert!(take_capture(first).is_none());
        assert!(matches!(
            current.ready.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        let capture = take_capture(second).unwrap();
        assert!(take_capture(second).is_none(), "one capture only");
        let _ = capture.send(Ok(iced::window::Screenshot::new(
            vec![1, 2, 3, 255],
            iced::Size::new(1, 1),
            1.0,
        )));
        assert_eq!(current.wait().await.unwrap().rgba.as_ref(), &[1, 2, 3, 255]);
        let canceled = next_draw(first);
        drop(canceled);
        assert!(DRAW_WAITER.lock().unwrap().pending.is_none());
        let timed = next_draw(first);
        assert!(matches!(
            timed.wait_for(Duration::ZERO).await,
            Err(SnapshotError::DrawTimeout)
        ));
        assert!(DRAW_WAITER.lock().unwrap().pending.is_none());
        let canceled = next_draw(first);
        drop(take_capture(first).unwrap());
        assert!(matches!(
            canceled.wait().await,
            Err(SnapshotError::CaptureCanceled)
        ));
        let oversized = next_draw(first);
        take_capture(first)
            .unwrap()
            .send(Err(SnapshotError::Oversized))
            .unwrap();
        assert!(matches!(
            oversized.wait().await,
            Err(SnapshotError::Oversized)
        ));
        assert_eq!(draws.load(std::sync::atomic::Ordering::Relaxed), 6);
        assert!(capture_size_allowed(iced::Size::new(3840, 2160)));
        assert!(!capture_size_allowed(iced::Size::new(u32::MAX, u32::MAX)));
        assert!(!capture_size_allowed(iced::Size::new(0, 100)));
    }
}

/// One invented onboarding fixture image, never a public CDN request.
/// The real bounded decoder still handles these bytes in fake mode.
pub(crate) fn onboarding_emoji(
    id: fastcord_model::Snowflake,
    animated: bool,
) -> Option<bytes::Bytes> {
    (id.0 == 900 && !animated).then(|| {
        bytes::Bytes::from_static(include_bytes!(
            "../../../fixtures/fake/onboarding-emoji.png"
        ))
    })
}

#[cfg(test)]
mod onboarding_fixture_tests {
    #[tokio::test]
    async fn static_onboarding_fixture_uses_real_bounded_decoder_without_network() {
        use fastcord_model::Snowflake;
        assert!(super::onboarding_emoji(Snowflake(901), false).is_none());
        assert!(super::onboarding_emoji(Snowflake(900), true).is_none());
        let bytes = super::onboarding_emoji(Snowflake(900), false).unwrap();
        let cache = crate::attachments::AttachmentCache::new(
            &fastcord_net::NetworkContext::off(),
            Snowflake(1),
            std::env::temp_dir().join(format!(
                "fastcord-onboarding-fixture-{}",
                std::process::id()
            )),
            "test",
        )
        .unwrap();
        let decoded = cache.decode_emoji(bytes).await.unwrap();
        assert_eq!(decoded.frames.len(), 1);
        assert_eq!(
            (decoded.frames[0].width, decoded.frames[0].height),
            (32, 32)
        );
        assert_eq!(decoded.frames[0].rgba.len(), 4096);
    }
}

#[cfg(test)]
mod dm_sort_tests {
    use super::*;
    #[test]
    fn native_dm_sort_script_uses_explicit_modes_and_no_message_write_steps() {
        use fastcord_discord::state::navigation::PrivateSort;
        let steps = parse_script(
            include_str!("../../../fixtures/ui/dm-sort.script"),
            std::path::Path::new("."),
        )
        .unwrap();
        let modes: Vec<_> = steps
            .iter()
            .filter_map(|(_, step)| match step {
                Step::DmSort(sort) => Some(*sort),
                _ => None,
            })
            .collect();
        assert_eq!(
            modes,
            [
                PrivateSort::Recent,
                PrivateSort::UnreadFirst,
                PrivateSort::Recent,
                PrivateSort::UnreadFirst
            ]
        );
        assert_eq!(
            steps
                .iter()
                .filter(|(_, s)| matches!(s, Step::Snap(..)))
                .count(),
            5
        );
        assert!(steps.iter().all(|(_, s)| matches!(
            s,
            Step::Open(_) | Step::DmSort(_) | Step::Snap(..) | Step::Quit
        )));
        assert!(
            parse_script("sort-dms unknown", std::path::Path::new("."))
                .unwrap()
                .is_empty()
        );
    }
}

#[cfg(test)]
mod community_list_tests {
    use super::*;
    #[test]
    fn community_list_native_fixture_only_opens_and_inspects() {
        let steps = parse_script(
            include_str!("../../../fixtures/ui/community-lists.script"),
            std::path::Path::new("."),
        )
        .unwrap();
        let panels: Vec<_> = steps
            .iter()
            .filter_map(|(_, step)| match step {
                Step::Toggle(panel) => Some(*panel),
                _ => None,
            })
            .collect();
        assert_eq!(
            panels,
            [
                "community-list-open",
                "community-list-onboarding",
                "community-list-question",
                "community-list-answer",
                "community-list-end",
                "community-list-end"
            ]
        );
        assert_eq!(
            steps
                .iter()
                .filter(|(_, step)| matches!(step, Step::Snap(..)))
                .count(),
            3
        );
        assert!(steps.iter().all(|(_, step)| matches!(
            step,
            Step::Open(_) | Step::Size(..) | Step::Toggle(_) | Step::Snap(..) | Step::Quit
        )));
    }
}

#[cfg(test)]
mod sidebar_nickname_tests {
    use super::*;

    #[test]
    fn native_sidebar_nickname_script_uses_real_text_channels_and_read_only_steps() {
        let world: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/fake/world.json")).unwrap();
        let steps = parse_script(
            include_str!("../../../fixtures/ui/sidebar-nickname.script"),
            std::path::Path::new("."),
        )
        .unwrap();
        let links: Vec<_> = steps
            .iter()
            .filter_map(|(_, step)| match step {
                Step::Open(link) => Some(link.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            links,
            [
                "discord://-/channels/5001/6005",
                "discord://-/channels/5002/6101",
                "discord://-/channels/@me/8003",
                "discord://-/channels/5001/6005"
            ]
        );
        for (guild, channel) in [("5001", "6005"), ("5002", "6101")] {
            let guild = world["guilds"]
                .as_array()
                .unwrap()
                .iter()
                .find(|g| g["id"] == guild)
                .unwrap();
            assert!(
                guild["channels"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c["id"] == channel && c["type"] == 0)
            );
        }
        assert_eq!(
            steps
                .iter()
                .filter(|(_, s)| matches!(s, Step::Snap(..)))
                .count(),
            4
        );
        assert!(steps.iter().all(|(_, s)| matches!(
            s,
            Step::Open(_) | Step::Size(..) | Step::Snap(..) | Step::Quit
        )));
    }
}

#[cfg(test)]
mod link_registration_fixture {
    use super::*;
    #[test]
    fn script_uses_only_navigation_and_simulated_registration() {
        let script = include_str!("../../../fixtures/ui/desktop-links.script");
        let steps =
            parse_script(script, std::path::Path::new("/tmp/fastcord-links-fixture")).unwrap();
        assert_eq!(
            steps
                .iter()
                .filter(|(_, step)| matches!(step, Step::Snap(..)))
                .count(),
            5
        );
        for (_, step) in steps {
            match step {
                Step::Open(link) => assert!(crate::desktop::parse(&link).is_some()),
                Step::Toggle(panel) => assert!(
                    [
                        "app",
                        "links-pending",
                        "links-conflict",
                        "links-register",
                        "escape"
                    ]
                    .contains(&panel)
                ),
                Step::Snap(..) | Step::Quit => {}
                _ => panic!("fixture must not join, send or call"),
            }
        }
    }
}
