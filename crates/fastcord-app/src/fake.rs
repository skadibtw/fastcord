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
        tokio::sync::oneshot::Sender<iced::window::Screenshot>,
    )>,
}

static DRAW_WAITER: std::sync::Mutex<DrawWaiter> = std::sync::Mutex::new(DrawWaiter {
    serial: 0,
    pending: None,
});

pub(crate) struct DrawTicket {
    serial: u64,
    ready: tokio::sync::oneshot::Receiver<iced::window::Screenshot>,
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
) -> Option<tokio::sync::oneshot::Sender<iced::window::Screenshot>> {
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
    pub(crate) async fn wait(self) -> Option<iced::window::Screenshot> {
        self.wait_for(Duration::from_secs(3)).await
    }

    async fn wait_for(mut self, timeout: Duration) -> Option<iced::window::Screenshot> {
        tokio::time::timeout(timeout, &mut self.ready)
            .await
            .ok()
            .and_then(Result::ok)
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
pub struct SnapshotAck(std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>);

impl PartialEq for SnapshotAck {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl SnapshotAck {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (send, ready) = tokio::sync::oneshot::channel();
        (
            Self(std::sync::Arc::new(std::sync::Mutex::new(Some(send)))),
            ready,
        )
    }

    pub(crate) fn finish(&self) {
        if let Some(send) = self.0.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = send.send(());
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
            (Step::Snap(path, Some(ack)), Some(ready))
        }
        step => (step, None),
    };
    if output.send(step).await.is_err() {
        return false;
    }
    if let Some(ready) = ready {
        // A dropped capture (including an aborted app task) releases the
        // barrier as a failure. A timeout stops further scripted mutation.
        if tokio::time::timeout(timeout, ready).await.is_err() {
            eprintln!("script: screenshot completion timed out; quitting fake app");
            let _ = output.send(Step::Quit).await;
            return false;
        }
    }
    true
}

/// Panels a script can open or close.
pub const PANELS: &[&str] = &[
    "search",
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
    "community-list-open",
    "community-list-onboarding",
    "community-list-question",
    "community-list-answer",
    "community-list-end",
    "onboarding-questions",
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

/// The script's steps with the pause before each; a bad line is skipped
/// with a message on standard error.
fn parse_script(text: &str, dir: &std::path::Path) -> Vec<(Duration, Step)> {
    let mut steps = Vec::new();
    let mut pause = Duration::ZERO;
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
        let rest = rest.trim();
        let step = match verb {
            "wait" => {
                match rest.parse::<f32>() {
                    Ok(seconds) if seconds >= 0.0 => pause += Duration::from_secs_f32(seconds),
                    _ => eprintln!("script: bad wait: {line}"),
                }
                continue;
            }
            "open" => Step::Open(rest.to_owned()),
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
    steps
}

/// The steps of `--script`, each after its pause; nothing without one.
pub fn script() -> impl iced::futures::Stream<Item = Step> {
    iced::stream::channel(1, async move |mut output| {
        let Some(path) = SCRIPT.get() else {
            std::future::pending::<()>().await;
            return;
        };
        let text = std::fs::read_to_string(path).unwrap_or_else(|_| {
            eprintln!("script: cannot read {}", path.display());
            String::new()
        });
        let dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_owned();
        for (pause, step) in parse_script(&text, &dir) {
            tokio::time::sleep(pause).await;
            if !send_step(&mut output, step, Duration::from_secs(10)).await {
                return;
            }
        }
        std::future::pending::<()>().await;
    })
}

/// Writes a window screenshot as an RGBA PNG.
pub fn save(path: &std::path::Path, shot: &iced::window::Screenshot) {
    let saved = image::save_buffer(
        path,
        &shot.rgba,
        shot.size.width,
        shot.size.height,
        image::ExtendedColorType::Rgba8,
    );
    match saved {
        Ok(()) => eprintln!("script: saved {}", path.display()),
        Err(error) => eprintln!("script: {}: {error}", path.display()),
    }
}

pub fn active() -> bool {
    PORT.get().is_some()
}

/// `--login-screen`: start on the sign-in screen instead of signing in.
pub fn login_screen() -> bool {
    std::env::args().any(|arg| arg == "--login-screen")
}

/// Reads `--fake-discord <port>` (a bare port, nothing else) and points the
/// Discord client at that loopback server. A malformed value ends the process.
pub fn init() {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--script" {
            if let Some(path) = args.next() {
                let _ = SCRIPT.set(PathBuf::from(path));
            }
            continue;
        }
        if arg != "--fake-discord" {
            continue;
        }
        let port = args.next().as_deref().and_then(parse_port);
        let Some(port) = port else {
            eprintln!("--fake-discord needs a bare port number, for example --fake-discord 8765");
            std::process::exit(2);
        };
        if fastcord_discord::fake_origin::set_origin(&format!("127.0.0.1:{port}")).is_err() {
            eprintln!("--fake-discord: the origin could not be set");
            std::process::exit(2);
        }
        let _ = PORT.set(port);
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
    fn scripts_parse_steps_with_their_pauses() {
        let dir = std::path::Path::new("shots");
        let steps = parse_script(
            "# comment\nwait 2\nopen discord://-/channels/@me\nsize 1080 672\nwait 0.5\nwait 0.5\nsnap home\nsnap ../x\nbogus\ntoggle pins\ntoggle nothing\njoin 5001 6009\nquit\n",
            dir,
        );
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

    #[tokio::test]
    async fn script_snapshot_blocks_next_action_until_saved_or_canceled() {
        use iced::futures::{FutureExt, StreamExt};
        for canceled in [false, true] {
            let (mut output, mut input) = iced::futures::channel::mpsc::channel(1);
            let driver = tokio::spawn(async move {
                assert!(
                    send_step(
                        &mut output,
                        Step::Snap(PathBuf::from("same.png"), None),
                        Duration::from_secs(1),
                    )
                    .await
                );
                assert!(send_step(&mut output, Step::Toggle("event-new"), Duration::ZERO).await);
            });
            let Some(Step::Snap(_, Some(ack))) = input.next().await else {
                panic!("script must register a request-specific completion before emission");
            };
            assert!(
                input.next().now_or_never().is_none(),
                "next UI mutation must wait"
            );
            if canceled {
                drop(ack); // aborted capture must not hang the script
            } else {
                ack.finish(); // FakeShot calls this only after save returns
            }
            assert_eq!(input.next().await, Some(Step::Toggle("event-new")));
            driver.await.unwrap();
        }
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
        old.finish();
        assert!(matches!(
            ready.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        current.finish();
        assert!(ready.await.is_ok());
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
        let _ = capture.send(iced::window::Screenshot::new(
            vec![1, 2, 3, 255],
            iced::Size::new(1, 1),
            1.0,
        ));
        assert_eq!(current.wait().await.unwrap().rgba.as_ref(), &[1, 2, 3, 255]);
        let canceled = next_draw(first);
        drop(canceled);
        assert!(DRAW_WAITER.lock().unwrap().pending.is_none());
        let timed = next_draw(first);
        assert!(timed.wait_for(Duration::ZERO).await.is_none());
        assert!(DRAW_WAITER.lock().unwrap().pending.is_none());
        assert_eq!(draws.load(std::sync::atomic::Ordering::Relaxed), 4);
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
mod community_list_tests {
    use super::*;
    #[test]
    fn community_list_native_fixture_only_opens_and_inspects() {
        let steps = parse_script(
            include_str!("../../../fixtures/ui/community-lists.script"),
            std::path::Path::new("."),
        );
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
