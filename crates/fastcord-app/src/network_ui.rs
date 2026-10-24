//! Network preferences share the per-user settings directory and fake namespace.
use fastcord_discord::UserToken;
use fastcord_net::proxy::{ProxyConfig, ProxyKind};
use fastcord_net::settings::{Mode, Settings};
use iced::widget::{button, checkbox, column, container, row, text, text_input};
use iced::{Element, Length};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct Loaded {
    pub settings: Settings,
    pub password: Arc<UserToken>,
    pub warning: Option<&'static str>,
}
impl Default for Loaded {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            password: Arc::new(UserToken::new(String::new())),
            warning: None,
        }
    }
}

#[derive(Default)]
pub struct State {
    pub open: bool,
    pub settings: Settings,
    pub notice: Option<&'static str>,
    pub saving: bool,
    pub proxy_kind: Option<ProxyKind>,
    pub proxy_host: String,
    pub proxy_port: String,
    pub proxy_username: String,
    pub proxy_password: Option<Arc<UserToken>>,
}

#[derive(Clone, Debug)]
pub enum Action {
    Open,
    Close,
    Retry,
    Retried,
    Mode(Mode),
    ExperimentalUdp(bool),
    Saved(Result<(), &'static str>),
    ProxyKind(ProxyKind),
    ProxyHost(String),
    ProxyPort(String),
    ProxyUsername(String),
    ProxyPassword(Arc<UserToken>),
    ApplyProxy,
    ProxyApplied(Result<Loaded, &'static str>),
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub async fn load() -> Loaded {
    let Some(dir) = crate::audio_devices::data_dir() else {
        return Loaded::default();
    };
    // Read through a bounded reader: metadata can change between stat and read.
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let blocked = || Loaded { settings: Settings { mode: Mode::Proxy, ..Settings::default() }, warning: Some("Network settings could not be read. Routing is stopped until you choose a mode."), ..Loaded::default() };
        let file = match std::fs::File::open(dir.join("network-settings.json")) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Loaded::default(),
            Err(_) => return blocked(),
        };
        let mut bytes = Vec::new();
        if file.take(64 * 1024 + 1).read_to_end(&mut bytes).is_err() { return blocked(); }
        let Ok(settings) = Settings::decode(&bytes, now()) else { return blocked(); };
        let mut loaded = Loaded { settings, ..Loaded::default() };
        if !crate::fake_mode() && let Some(config) = loaded.settings.proxy.as_ref().filter(|config| config.username.is_some()) {
            let password = fastcord_platform::NativeCredentialStore::new().and_then(|store| store.load_proxy_password(&config.credential_key()));
            match password {
                Ok(Some(password)) => { let mut secret = password.into_secret(); loaded.password = Arc::new(UserToken::new(std::mem::take(&mut *secret))); }
                _ => loaded.warning = Some("The proxy password could not be loaded. Open Network recovery to save it again."),
            }
        }
        loaded
    })
    .await
    .unwrap_or_else(|_| Loaded { settings: Settings { mode: Mode::Proxy, ..Settings::default() }, warning: Some("Network settings could not be read; routing is stopped."), ..Loaded::default() })
}

impl State {
    pub fn apply(&mut self, loaded: &Loaded) {
        self.settings = loaded.settings.clone();
        self.notice = loaded.warning;
        self.proxy_password = Some(loaded.password.clone());
        if let Some(config) = &loaded.settings.proxy {
            self.proxy_kind = Some(config.kind);
            self.proxy_host = config.host.clone();
            self.proxy_port = config.port.to_string();
            self.proxy_username = config.username.clone().unwrap_or_default();
        }
    }
    pub fn proxy_config(&self) -> Result<ProxyConfig, &'static str> {
        let config = ProxyConfig {
            kind: self.proxy_kind.unwrap_or(ProxyKind::Http),
            host: self.proxy_host.trim().to_owned(),
            port: self
                .proxy_port
                .parse()
                .map_err(|_| "Enter a proxy port from 1 to 65535.")?,
            username: (!self.proxy_username.is_empty()).then(|| self.proxy_username.clone()),
        };
        config
            .validate()
            .map_err(|_| "The proxy host, port or username is invalid.")?;
        Ok(config)
    }
}

pub fn configure(
    context: &fastcord_net::NetworkContext,
    loaded: &Loaded,
) -> Result<fastcord_net::NetworkContext, fastcord_net::NetworkError> {
    let context = if loaded.settings.mode == Mode::Proxy {
        let config = loaded
            .settings
            .proxy
            .clone()
            .ok_or(fastcord_net::NetworkError::ProxyRefused)?;
        if loaded.warning.is_some() {
            return Err(fastcord_net::NetworkError::ProxyRefused);
        }
        context.with_proxy(
            config,
            zeroize::Zeroizing::new(loaded.password.expose_secret().to_owned()),
        )?
    } else {
        context.with_mode(loaded.settings.mode)
    };
    Ok(context.with_experimental_udp(loaded.settings.experimental_udp))
}

pub async fn apply_proxy(
    settings: Settings,
    password: Arc<UserToken>,
) -> Result<Loaded, &'static str> {
    let config = settings
        .proxy
        .as_ref()
        .ok_or("Enter a proxy first.")?
        .clone();
    let saving_password = password.clone();
    tokio::task::spawn_blocking(move || {
        if crate::fake_mode() && config.username.is_some() {
            return Err("Proxy passwords are unavailable in fake mode.");
        }
        if config.username.is_some() {
            let store = fastcord_platform::NativeCredentialStore::new().map_err(
                |_| "Native credential storage is unavailable; the proxy was not applied.",
            )?;
            store
                .save_proxy_password(&config.credential_key(), saving_password.expose_secret())
                .map_err(|_| "The proxy password could not be saved.")?;
        }
        Ok(())
    })
    .await
    .map_err(|_| "The proxy password could not be saved.")??;
    save(settings.clone()).await?;
    Ok(Loaded {
        settings,
        password,
        warning: None,
    })
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
        checkbox(state.settings.experimental_udp)
            .label("Experimental UDP discovery preludes (unverified)")
            .on_toggle_maybe((editable && !state.saving && state.settings.mode != Mode::Proxy).then_some(|value| crate::Message::Network(Action::ExperimentalUdp(value)))),
        text(if state.settings.mode == Mode::Proxy { "Experimental preludes are disabled in Proxy mode. SOCKS5 UDP uses ordinary discovery." } else { "Separate opt-in for direct media routes. Tries up to three UDP flows before protocol selection. Effectiveness on filtered networks is unverified; proxy routing does not enable it." }),
        text("Auto checks only a required service after an eligible connection failure. TLS reachability does not establish that Discord or media works. Recovery is bounded; it never bypasses certificate verification."),
        text("TLS splitting is best effort. TCP writes do not guarantee packet boundaries. UDP media has a separate connection path."),
        text(if editable { "Changes apply to the next connection." } else { "Sign out before changing routing. Healthy sessions remain connected." }),
        button("Retry pending TLS check").on_press(crate::Message::Network(Action::Retry)),
        button("Close").on_press(crate::Message::Network(Action::Close)),
    ].spacing(12);
    let password = state
        .proxy_password
        .as_ref()
        .map_or("", |password| password.expose_secret());
    let proxy_choice = |label, kind| {
        button(label).on_press_maybe(
            (editable && !state.saving).then_some(crate::Message::Network(Action::ProxyKind(kind))),
        )
    };
    body = body.push(text("Proxy").size(22))
        .push(row![proxy_choice("HTTP CONNECT", ProxyKind::Http), proxy_choice("HTTPS CONNECT", ProxyKind::Https), proxy_choice("SOCKS5", ProxyKind::Socks5)].spacing(8))
        .push(text(format!("Type: {}", state.proxy_kind.unwrap_or(ProxyKind::Http))))
        .push(text_input("Proxy host (no scheme or path)", &state.proxy_host).on_input_maybe((editable && !state.saving).then_some(|value| crate::Message::Network(Action::ProxyHost(value)))))
        .push(text_input("Proxy port", &state.proxy_port).on_input_maybe((editable && !state.saving).then_some(|value| crate::Message::Network(Action::ProxyPort(value)))))
        .push(text_input("Username (optional)", &state.proxy_username).on_input_maybe((editable && !state.saving).then_some(|value| crate::Message::Network(Action::ProxyUsername(value)))))
        .push(text_input("Password", password).secure(true).on_input_maybe((editable && !state.saving).then_some(|value| crate::Message::Network(Action::ProxyPassword(Arc::new(UserToken::new(value)))))))
        .push(button("Save and use proxy").on_press_maybe((editable && !state.saving).then_some(crate::Message::Network(Action::ApplyProxy))))
        .push(text("Proxy mode never falls back to a direct connection. CONNECT proxies carry TCP only: voice, camera and screen sharing cannot join through them. SOCKS5 UDP requires a UDP-capable proxy and a working UDP route to that proxy. It does not carry media over TCP and does not encrypt the client-to-proxy transport. Passwords are saved in native credential storage."));
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

pub(crate) fn status_text(status: fastcord_net::selector::ServiceStatus) -> String {
    use fastcord_net::selector::ServiceStatus;
    match status {
        ServiceStatus::Unknown => "Not checked".into(),
        ServiceStatus::Selecting => "Checking TLS routes…".into(),
        ServiceStatus::TlsReachable => "TLS reachable; service not confirmed".into(),
        ServiceStatus::ServiceReachable => "Service answered".into(),
        ServiceStatus::SessionHealthy => "Session connected".into(),
        ServiceStatus::UdpUnreachable => "UDP media did not answer; TLS is separate".into(),
        ServiceStatus::UdpDiscovering { variant } => {
            format!("Experimental UDP discovery: variant {variant}/3")
        }
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

/// Credential-free reachability check under the actual fastcord executable's
/// routing rules. It runs before settings, account restore and instance handoff.
#[cfg(debug_assertions)]
pub fn check() {
    let worker = std::thread::Builder::new()
        .name("fastcord-network-check".into())
        .spawn(|| {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                eprintln!("Network check: runtime could not start.");
                return;
            };
            runtime.block_on(async {
                use fastcord_net::Role;
                use std::time::Duration;
                for mode in [Mode::Off, Mode::Auto] {
                    let context = fastcord_net::NetworkContext::off().with_mode(mode);
                    let client = context.http_builder(Role::Rest).and_then(|builder| {
                        builder
                            .connect_timeout(Duration::from_secs(8))
                            .timeout(Duration::from_secs(12))
                            .build()
                    });
                    let Ok(client) = client else {
                        eprintln!("{mode:?}: HTTP client could not start.");
                        continue;
                    };
                    match client
                        .get("https://discord.com/api/v9/gateway")
                        .send()
                        .await
                    {
                        Ok(response) => println!(
                            "{mode:?}: anonymous gateway HTTP {}",
                            response.status().as_u16()
                        ),
                        Err(error) => println!(
                            "{mode:?}: category={:?}; connect={}; timeout={}",
                            error.category(),
                            error.is_connect(),
                            error.is_timeout()
                        ),
                    }
                    println!(
                        "{mode:?}: service={:?}",
                        context.controller().service_status(Role::Rest)
                    );
                    if mode == Mode::Auto {
                        context.wait_for_recovery().await;
                        println!(
                            "Auto after bounded TLS check: {:?}",
                            context.controller().service_status(Role::Rest)
                        );
                    }
                }
            });
        });
    if !worker.is_ok_and(|worker| worker.join().is_ok()) {
        eprintln!("Network check: worker stopped.");
    }
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
                notice: Some(
                    "Offline preview: changes stay in memory; no account or network traffic.",
                ),
                ..State::default()
            },
            context: fastcord_net::NetworkContext::off(),
        },
        |state: &mut Preview, message: crate::Message| {
            if let crate::Message::Network(action) = message {
                match action {
                    Action::Mode(mode) => {
                        state.state.settings.mode = mode;
                        state.context = state.context.with_mode(mode);
                    }
                    Action::ExperimentalUdp(enabled) => {
                        state.state.settings.experimental_udp = enabled;
                        state.context = state.context.with_experimental_udp(enabled);
                    }
                    Action::ProxyKind(kind) => state.state.proxy_kind = Some(kind),
                    Action::ProxyHost(value) => state.state.proxy_host = value,
                    Action::ProxyPort(value) => state.state.proxy_port = value,
                    Action::ProxyUsername(value) => state.state.proxy_username = value,
                    Action::ProxyPassword(value) => state.state.proxy_password = Some(value),
                    _ => {}
                }
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
