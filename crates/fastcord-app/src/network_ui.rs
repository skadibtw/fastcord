//! Network preferences share the per-user settings directory and fake namespace.
use fastcord_net::settings::{Mode, Settings};
use iced::widget::{button, column, container, row, text};
use iced::{Element, Length};

#[derive(Default)]
pub struct State {
    pub open: bool,
    pub settings: Settings,
    pub notice: Option<&'static str>,
    pub saving: bool,
}

#[derive(Clone, Debug)]
pub enum Action {
    Open,
    Close,
    Retry,
    Retried,
    Mode(Mode),
    Saved(Result<(), &'static str>),
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub async fn load() -> Settings {
    let Some(dir) = crate::audio_devices::data_dir() else {
        return Settings::default();
    };
    // Read through a bounded reader: metadata can change between stat and read.
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let file = std::fs::File::open(dir.join("network-settings.json")).ok()?;
        let mut bytes = Vec::new();
        file.take(64 * 1024 + 1).read_to_end(&mut bytes).ok()?;
        Settings::decode(&bytes, now()).ok()
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

pub async fn save(settings: Settings) -> Result<(), &'static str> {
    tokio::task::spawn_blocking(move || {
        let bytes = settings
            .encode(now())
            .map_err(|_| "Network settings are invalid.")?;
        let dir = crate::audio_devices::data_dir().ok_or("The settings folder is unavailable.")?;
        std::fs::create_dir_all(&dir).map_err(|_| "Network settings could not be saved.")?;
        let pending = dir.join("network-settings.json.pending");
        std::fs::write(&pending, bytes).map_err(|_| "Network settings could not be saved.")?;
        std::fs::rename(pending, dir.join("network-settings.json"))
            .map_err(|_| "Network settings could not be saved.")
    })
    .await
    .unwrap_or(Err("Network settings could not be saved."))
}

pub fn view<'a>(
    state: &'a State,
    context: &fastcord_net::NetworkContext,
    editable: bool,
) -> Element<'a, crate::Message> {
    let choose = |label, mode| {
        button(label).on_press_maybe(
            (editable && !state.saving).then_some(crate::Message::Network(Action::Mode(mode))),
        )
    };
    let mut body = column![
        text("Network recovery").size(26),
        text(format!("Mode: {:?}", state.settings.mode)),
        row![choose("Off", Mode::Off), choose("Auto", Mode::Auto)].spacing(12),
        text("Auto checks only a required service after an eligible connection failure. TLS reachability does not establish that Discord or media works. Recovery is bounded; it never bypasses certificate verification."),
        text("TLS splitting is best effort. TCP writes do not guarantee packet boundaries. UDP media has a separate connection path."),
        text(if editable { "Changes apply to the next connection." } else { "Sign out before changing routing. Healthy sessions remain connected." }),
        button("Retry pending TLS check").on_press(crate::Message::Network(Action::Retry)),
        button("Close").on_press(crate::Message::Network(Action::Close)),
    ].spacing(12);
    for (label, role) in [
        ("API", fastcord_net::Role::Rest),
        ("Gateway", fastcord_net::Role::Gateway),
        ("QR login", fastcord_net::Role::RemoteAuth),
        ("Login", fastcord_net::Role::Login),
        ("Profile", fastcord_net::Role::Profile),
        ("CDN", fastcord_net::Role::Cdn),
        ("Uploads", fastcord_net::Role::Upload),
        ("Voice", fastcord_net::Role::Voice),
        ("Streams", fastcord_net::Role::Stream),
    ] {
        body = body.push(text(format!(
            "{label}: {}",
            status_text(context.controller().service_status(role))
        )));
    }
    if let Some(notice) = state.notice {
        body = body.push(text(notice));
    }
    container(body)
        .padding(24)
        .width(Length::Fixed(650.0))
        .into()
}

fn status_text(status: fastcord_net::selector::ServiceStatus) -> String {
    use fastcord_net::selector::ServiceStatus;
    match status {
        ServiceStatus::Unknown => "Not checked".into(),
        ServiceStatus::Selecting => "Checking TLS routes…".into(),
        ServiceStatus::TlsReachable => "TLS reachable; service not confirmed".into(),
        ServiceStatus::ServiceReachable => "Service answered".into(),
        ServiceStatus::SessionHealthy => "Session connected".into(),
        ServiceStatus::UdpUnreachable => "UDP media did not answer; TLS is separate".into(),
        ServiceStatus::Failed(error) => error.to_string(),
    }
}

pub struct Updates(pub fastcord_net::selector::Controller);
impl std::hash::Hash for Updates {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // One controller lives for the whole app process, across mode/account changes.
        std::hash::Hash::hash(&"network-controller-updates", state);
    }
}
pub fn updates(spec: &Updates) -> impl iced::futures::Stream<Item = ()> + use<> {
    iced::futures::stream::unfold(spec.0.changes(), |mut receiver| async move {
        receiver.changed().await.ok()?;
        Some(((), receiver))
    })
}

/// Native hints are events. The debounce exists only while a change is pending.
pub fn changes() -> impl iced::futures::Stream<Item = ()> {
    iced::stream::channel(
        1,
        |mut output: iced::futures::channel::mpsc::Sender<()>| async move {
            use iced::futures::SinkExt;
            let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
            let _watcher = fastcord_platform::network::NetworkChanges::start(move || {
                let _ = sender.try_send(());
            });
            loop {
                let Some(()) = receiver.recv().await else {
                    std::future::pending::<()>().await;
                    unreachable!()
                };
                let quiet = tokio::time::sleep(std::time::Duration::from_millis(500));
                tokio::pin!(quiet);
                loop {
                    tokio::select! {
                        _ = &mut quiet => break,
                        value = receiver.recv() => {
                            if value.is_none() { break; }
                            quiet.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_millis(500));
                        }
                    }
                }
                if output.send(()).await.is_err() {
                    break;
                }
            }
        },
    )
}

/// Real binary/UI check without credential access, restoration or network I/O.
#[cfg(debug_assertions)]
pub fn preview() -> iced::Result {
    struct Preview {
        state: State,
        context: fastcord_net::NetworkContext,
    }
    fn view(state: &Preview) -> Element<'_, crate::Message> {
        iced::widget::center(iced::widget::scrollable(super::network_ui::view(
            &state.state,
            &state.context,
            true,
        )))
        .into()
    }
    crate::render::application(
        || Preview {
            state: State {
                open: true,
                ..State::default()
            },
            context: fastcord_net::NetworkContext::off(),
        },
        |state: &mut Preview, message: crate::Message| {
            if let crate::Message::Network(Action::Mode(mode)) = message {
                state.state.settings.mode = mode;
                state.context = state.context.with_mode(mode);
            }
        },
        view,
    )
    .title("fastcord — offline Network preview")
    .theme(|_: &Preview| crate::style::theme())
    .window(iced::window::Settings {
        size: iced::Size::new(820.0, 800.0),
        icon: crate::icons::window_icon(),
        ..Default::default()
    })
    .run()
}
