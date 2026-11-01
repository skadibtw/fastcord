//! Network preferences share the per-user settings directory and fake namespace.
use crate::i18n::tr;
use crate::ui::button;
use fastcord_discord::UserToken;
use fastcord_net::proxy::{ProxyConfig, ProxyKind};
use fastcord_net::settings::{Mode, Settings};
use iced::widget::{column, container, row, text, text_input};
use iced::{Element, Length};
use std::sync::Arc;

pub fn new_context() -> fastcord_net::NetworkContext {
    fastcord_net::NetworkContext::off()
}

/// Explicit manual alt-account diagnostic. A bounded private stdin pipe is the
/// only token input; no native store, settings, UI restore or account output.
#[cfg(debug_assertions)]
pub fn check_login() {
    use fastcord_discord::gateway::{ConnectionState, Dispatch, Gateway, GatewayEvent};
    use std::io::Read;
    use std::time::Duration;
    let mut bytes = zeroize::Zeroizing::new(Vec::new());
    if std::io::stdin().take(4097).read_to_end(&mut bytes).is_err() || bytes.len() > 4096 {
        println!("Login check: invalid input.");
        return;
    }
    let Ok(input) = std::str::from_utf8(&bytes) else {
        println!("Login check: invalid input.");
        return;
    };
    let token = Arc::new(UserToken::new(input.trim().to_owned()));
    drop(bytes);
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        println!("Login check: runtime unavailable.");
        return;
    };
    runtime.block_on(async {
        // This diagnostic exercises the built-in route. It must never open
        // UAC or start the separately requested optional packet helper.
        let context = new_context().with_mode(Mode::Auto);
        let validated = tokio::time::timeout(
            Duration::from_secs(40),
            crate::login::validate(context.clone(), token, None),
        )
        .await;
        let session = match validated {
            Ok(Ok(session)) => session,
            Ok(Err(error)) => {
                println!("Login validation: {}", error.notice(&context));
                return;
            }
            Err(_) => {
                println!("Login validation: deadline exceeded.");
                return;
            }
        };
        println!("Authenticated login validation passed.");
        let mut gateway = Gateway::start(session.client.clone(), session.token());
        let closer = gateway.closer();
        let result = tokio::time::timeout(Duration::from_secs(25), async {
            while let Some(event) = gateway.next_event().await {
                match event {
                    GatewayEvent::Dispatch {
                        event: Dispatch::Ready(_),
                        ..
                    } => return true,
                    GatewayEvent::State(
                        ConnectionState::AuthenticationRequired
                        | ConnectionState::Stopped(_)
                        | ConnectionState::Reconnecting { .. },
                    ) => return false,
                    _ => {}
                }
            }
            false
        })
        .await;
        println!("Gateway READY received: {}", result == Ok(true));
        println!(
            "Gateway clean close: {}",
            closer.close(Duration::from_secs(3)).await
        );
        drop(gateway);
        drop(session);
    });
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchTicket {
    generation: u64,
    serial: u64,
}

#[derive(Default)]
pub struct State {
    pub open: bool,
    /// The technical explanations are unfolded.
    pub details: bool,
    pub settings: Settings,
    pub notice: Option<&'static str>,
    pub saving: bool,
    search: Option<SearchTicket>,
    pub source_address: String,
    pub packet_active: bool,
    pub profile_digest: Option<String>,
    profile_generation: Option<u64>,
    profile_revision: u64,
    profile_dirty: bool,
    initialized: bool,
    pub proxy_kind: Option<ProxyKind>,
    pub proxy_host: String,
    pub proxy_port: String,
    pub proxy_username: String,
    pub proxy_password: Option<Arc<UserToken>>,
}

#[derive(Clone, Debug)]
pub enum Action {
    ProfileChanged(Option<String>),
    HintsSaved(Result<(), &'static str>),
    Open,
    Details(bool),
    Retry,
    Retried,
    FullSearch,
    SourceAddress(String),
    ApplySourceAddress,
    FullSearchFinished {
        ticket: SearchTicket,
        outcome: fastcord_net::ExtendedSearchOutcome,
    },
    Mode(Mode),
    Saved(Result<(), &'static str>),
    ProxyKind(ProxyKind),
    ProxyHost(String),
    ProxyPort(String),
    ProxyUsername(String),
    ProxyPassword(Arc<UserToken>),
    ApplyProxy,
    ProxyApplied(Result<Loaded, &'static str>),
    PacketStart,
    PacketStarted(Result<PacketRoute, &'static str>),
    PacketStop,
    PacketCheck,
    PacketChecked(&'static str),
}

#[derive(Clone)]
pub struct PacketRoute(pub Arc<dyn fastcord_net::tls::FakePrefixFactory>);
impl std::fmt::Debug for PacketRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PacketRoute")
    }
}
pub async fn start_packet() -> Result<PacketRoute, &'static str> {
    tokio::task::spawn_blocking(|| {
        #[cfg(target_os = "windows")]
        { fastcord_platform::network::PacketHelper::launch().map(|helper| PacketRoute(Arc::new(fastcord_platform::network::PacketFactory(helper)))).map_err(|_| "Packet recovery could not start. UAC may have been cancelled, or the pinned WinDivert driver is unavailable.") }
        #[cfg(not(target_os = "windows"))]
        { Err("Packet recovery is currently available on Windows only.") }
    }).await.unwrap_or(Err("Packet recovery worker stopped."))
}
pub async fn packet_check(context: fastcord_net::NetworkContext) -> &'static str {
    use std::time::Duration;
    let mut packet_tried = false;
    let mut helper_available = context.packet_helper_available();
    for attempt in 0..2 {
        let Ok(builder) = context.http_builder(fastcord_net::Role::Rest) else {
            return "Anonymous check: HTTP client could not start.";
        };
        let (result, evidence) = builder
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(12))
            .anonymous_get("https://discord.com/api/v9/gateway")
            .await;
        packet_tried |= evidence.packet_attempted;
        helper_available = evidence.helper_available;
        match result {
            Ok(response) if response.status().as_u16() == 200 => {
                return "Anonymous Discord API check passed (HTTP 200). You can try signing in.";
            }
            Ok(_) => {
                return "Discord returned an HTTP response; the anonymous API check did not return 200.";
            }
            Err(error) => {
                if attempt != 0
                    || context.mode() != Mode::Auto
                    || !error.category().eligible_for_selection()
                {
                    break;
                }
                // Only one credential-free retry after real recovery evidence.
                // Selecting an available helper is not a transport attempt.
                context.wait_for_recovery().await;
                if context
                    .controller()
                    .service_status(fastcord_net::Role::Rest)
                    != fastcord_net::selector::ServiceStatus::TlsReachable
                {
                    break;
                }
            }
        }
    }
    if context.mode() == Mode::Auto && helper_available && !packet_tried {
        "Anonymous Discord API check failed with TLS recovery. The packet helper was available but was not attempted by this request."
    } else {
        check_failed(context.mode(), packet_tried)
    }
}

/// Names the route the failed check took, so it never blames a helper that
/// was not running (#218).
fn check_failed(mode: Mode, packet: bool) -> &'static str {
    match mode {
        Mode::Off => {
            "Anonymous Discord API check failed on the direct route. Recovery is off: choose Automatic to try other TLS shapes."
        }
        Mode::Auto if packet => {
            "Anonymous Discord API check failed. Packet recovery has not restored access."
        }
        Mode::Auto => {
            "Anonymous Discord API check failed with TLS recovery. The packet helper is not running."
        }
        Mode::Proxy => "Anonymous Discord API check failed through the proxy.",
    }
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
    pub fn search_busy(&self) -> bool {
        self.search.is_some()
    }

    pub fn begin_search(&mut self, generation: u64) -> SearchTicket {
        // Process-wide serials survive a settings-state reload/replacement.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let ticket = SearchTicket {
            generation,
            serial: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        };
        self.search = Some(ticket);
        ticket
    }

    pub fn finish_search(&mut self, ticket: SearchTicket, generation: u64) -> bool {
        if self.search != Some(ticket) {
            return false;
        }
        self.search = None;
        ticket.generation == generation
    }

    pub fn apply_source(
        &self,
        context: &fastcord_net::NetworkContext,
    ) -> Result<fastcord_net::NetworkContext, &'static str> {
        let text = self.source_address.trim();
        let address = if text.is_empty() {
            None
        } else {
            Some(text.parse::<std::net::IpAddr>().map_err(
                |_| "Enter a numeric local IP address, without a port or interface name.",
            )?)
        };
        context
            .with_source_address(address)
            .map_err(|_| "Choose a specific unicast local address, or leave the field empty.")
    }

    pub fn apply(&mut self, loaded: &Loaded) {
        self.settings = loaded.settings.clone();
        self.initialized = true;
        self.profile_generation = None;
        self.notice = loaded.warning;
        self.proxy_password = Some(loaded.password.clone());
        if let Some(config) = &loaded.settings.proxy {
            self.proxy_kind = Some(config.kind);
            self.proxy_host = config.host.clone();
            self.proxy_port = config.port.to_string();
            self.proxy_username = config.username.clone().unwrap_or_default();
        }
    }
    pub fn bind_profile(&mut self, context: &fastcord_net::NetworkContext) {
        if !self.initialized {
            return;
        }
        let generation = context.controller().generation();
        if self.profile_generation == Some(generation) {
            return;
        }
        let hints = if context.mode() == Mode::Auto {
            self.profile_digest
                .as_ref()
                .and_then(|digest| {
                    self.settings
                        .profiles
                        .iter()
                        .find(|p| &p.local_network_digest == digest)
                })
                .map_or(&[][..], |p| p.hints.as_slice())
        } else {
            &[][..]
        };
        context
            .controller()
            .load_profile_hints(generation, hints, now());
        self.profile_generation = Some(generation);
        self.profile_revision = context.controller().profile_hints().1;
    }

    pub fn collect_hints(&mut self, context: &fastcord_net::NetworkContext) -> Option<Settings> {
        self.bind_profile(context);
        if !self.initialized || context.mode() != Mode::Auto {
            return None;
        }
        let digest = self.profile_digest.as_ref()?;
        let (generation, revision, hints) = context.controller().profile_hints();
        if self.profile_generation != Some(generation) {
            return None;
        }
        if revision != self.profile_revision {
            self.settings
                .profiles
                .retain(|p| &p.local_network_digest != digest);
            for hint in hints {
                if self
                    .settings
                    .remember_hint(digest, hint.role, hint.strategy(), now())
                    .is_ok()
                    && let Some(saved) = self
                        .settings
                        .profiles
                        .iter_mut()
                        .find(|p| &p.local_network_digest == digest)
                        .and_then(|p| p.hints.iter_mut().find(|h| h.role == hint.role))
                {
                    saved.confirmed_at_unix_secs = hint.confirmed_at_unix_secs;
                }
            }
            self.profile_revision = revision;
            self.profile_dirty = true;
        }
        if self.profile_dirty && !self.saving {
            self.profile_dirty = false;
            self.saving = true;
            Some(self.settings.clone())
        } else {
            None
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
    // The saved experimental UDP flag remains disabled until a candidate is
    // validated on a filtered path.
    Ok(context)
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
        fastcord_platform::paths::write_atomic(&dir.join("network-settings.json"), &bytes)
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
    view_with_capability(
        state,
        context,
        editable,
        fastcord_platform::network::packet_capabilities().tcp_fake,
    )
}

fn view_with_capability<'a>(
    state: &'a State,
    context: &fastcord_net::NetworkContext,
    editable: bool,
    native_helper: bool,
) -> Element<'a, crate::Message> {
    use crate::i18n::tr;
    use crate::{style, ui};
    let open = editable && !state.saving;
    let act = move |action: Action| open.then_some(crate::Message::Network(action));
    let choice = |label: &'a str, selected: bool, on: Option<crate::Message>| {
        button(text(label).size(14))
            .on_press_maybe(on.filter(|_| !selected))
            .padding([7, 14])
            .style(style::secondary_button(selected))
    };
    let section = |title: &'a str| text(title).size(16).font(ui::BOLD);

    let mut body = column![].spacing(12);
    // Where things stand, then why (an error or a check result) if anything.
    body = body.push(
        column![
            text(path_text(
                state.settings.mode,
                state.packet_active && context.packet_helper_available(),
                state.proxy_kind
            ))
            .size(14),
            ui::note(if editable {
                tr("Changes apply to the next connection.")
            } else {
                tr("Sign out to change routing. Connected sessions are not interrupted.")
            }),
        ]
        .spacing(4),
    );
    if let Some(notice) = state.notice {
        body = body.push(ui::callout(text(tr(notice)).size(13)));
    }

    // Recovery mode.
    body = body.push(section(tr("Connection recovery"))).push(
        row![
            choice(
                tr("Off"),
                state.settings.mode == Mode::Off,
                act(Action::Mode(Mode::Off))
            ),
            choice(
                tr("Automatic"),
                state.settings.mode == Mode::Auto,
                act(Action::Mode(Mode::Auto))
            ),
        ]
        .spacing(6),
    );
    body = body.push(ui::note(tr(
        "Automatic retries a blocked secure connection by another route. Certificates are always checked.",
    )));
    body = body.push(section(tr("Local source address (this session)"))).push(
        row![
            text_input("Automatic, or a numeric local IP", &state.source_address)
                .on_input_maybe(open.then_some(|value| crate::Message::Network(Action::SourceAddress(value)))),
            button(text(tr("Apply"))).on_press_maybe(act(Action::ApplySourceAddress)),
        ].spacing(6)
    ).push(ui::note(tr(if context.mode() == Mode::Proxy {
        "Proxy mode ignores the direct source address. The proxy connection and relay choose their own source."
    } else if context.source_address().is_some() {
        "A direct source address is configured. Bind failures stop the connection; no automatic unbound fallback. This does not select an interface or bypass VPN/TUN routing."
    } else {
        "Direct connections use the system-selected source. A local address does not select an interface or bypass VPN/TUN routing."
    })));
    if native_helper {
        body = body.push(
            row![
                button(
                    text(if state.packet_active {
                        tr("Stop packet helper")
                    } else {
                        tr("Start packet helper")
                    })
                    .size(14),
                )
                .on_press_maybe(act(if state.packet_active {
                    Action::PacketStop
                } else {
                    Action::PacketStart
                }))
                .padding([7, 14])
                .style(style::secondary_button(false)),
            ]
            .spacing(6)
            .wrap(),
        );
        body = body.push(ui::note(if state.packet_active && !context.packet_helper_available() {
            tr("The packet helper stopped. Sign out and restart it to retry packet recovery.")
        } else if state.packet_active {
            tr("The packet helper is on for this session.")
        } else {
            tr(
                "The packet helper asks for administrator rights once per session and selects Automatic.",
            )
        }));
    }

    body = body.push(
        button(text(tr("Check connection")).size(14))
            .on_press_maybe(act(Action::PacketCheck))
            .padding([7, 14])
            .style(style::secondary_button(false)),
    );
    if !native_helper {
        body = body.push(ui::note(tr(
            "Native packet recovery is unsupported on this platform. Automatic TLS recovery and configured proxies remain available.",
        )));
    }
    body = body.push(ui::note(tr(if state.profile_digest.is_some() {
        "Strategy preferences are saved for this connection profile and checked again before use."
    } else {
        "Network strategies will be remembered only for this session."
    })));

    // Proxy.
    let password = state
        .proxy_password
        .as_ref()
        .map_or("", |password| password.expose_secret());
    let kind = state.proxy_kind.unwrap_or(ProxyKind::Http);
    let field = |label: &'a str, input: iced::widget::TextInput<'a, crate::Message>| {
        column![ui::label(label), input.padding(8).size(14)].spacing(4)
    };
    body = body
        .push(section(tr("Proxy")))
        .push(
            row![
                choice(
                    "HTTP CONNECT",
                    kind == ProxyKind::Http,
                    act(Action::ProxyKind(ProxyKind::Http))
                ),
                choice(
                    "HTTPS CONNECT",
                    kind == ProxyKind::Https,
                    act(Action::ProxyKind(ProxyKind::Https))
                ),
                choice(
                    "SOCKS5",
                    kind == ProxyKind::Socks5,
                    act(Action::ProxyKind(ProxyKind::Socks5))
                ),
            ]
            .spacing(6)
            .wrap(),
        )
        .push(
            row![
                container(field(
                    tr("Host"),
                    text_input("proxy.example.com", &state.proxy_host).on_input_maybe(
                        open.then_some(|value| crate::Message::Network(Action::ProxyHost(value)))
                    ),
                ))
                .width(Length::FillPortion(3)),
                container(field(
                    tr("Port"),
                    text_input("1080", &state.proxy_port).on_input_maybe(
                        open.then_some(|value| crate::Message::Network(Action::ProxyPort(value)))
                    ),
                ))
                .width(Length::FillPortion(1)),
            ]
            .spacing(10),
        )
        .push(
            row![
                container(field(
                    tr("Username (optional)"),
                    text_input("", &state.proxy_username).on_input_maybe(open.then_some(
                        |value| crate::Message::Network(Action::ProxyUsername(value))
                    )),
                ))
                .width(Length::Fill),
                container(field(
                    tr("Password"),
                    text_input("", password)
                        .secure(true)
                        .on_input_maybe(open.then_some(|value| {
                            crate::Message::Network(Action::ProxyPassword(Arc::new(
                                UserToken::new(value),
                            )))
                        })),
                ))
                .width(Length::Fill),
            ]
            .spacing(10),
        )
        .push(
            button(text(tr("Save and use proxy")).size(14).font(ui::BOLD))
                .on_press_maybe(act(Action::ApplyProxy))
                .padding([8, 16])
                .style(style::action_button(false)),
        )
        .push(ui::note(tr(
            "A proxy is never bypassed. Voice and video need SOCKS5 with UDP. The password is kept in the system's credential store.",
        )));

    // Media.
    body = body.push(section(tr("Voice and video"))).push(ui::note(tr(
        "Voice and video use UDP. Automatic mode retries when a voice server does not answer; some networks may still block voice.",
    )));

    // Each service's state, short.
    let mut services = column![].spacing(4);
    for (label, role) in [
        ("API", fastcord_net::Role::Rest),
        ("Gateway", fastcord_net::Role::Gateway),
        ("QR login", fastcord_net::Role::RemoteAuth),
        ("Login", fastcord_net::Role::Login),
        ("Profile", fastcord_net::Role::Profile),
        ("CDN", fastcord_net::Role::Cdn),
        ("Uploads", fastcord_net::Role::Upload),
        ("Voice calls", fastcord_net::Role::Voice),
        ("Streams", fastcord_net::Role::Stream),
    ] {
        services = services.push(
            row![
                text(tr(label)).size(13).width(140),
                column![
                    text(status_text(context.controller().service_status(role)))
                        .size(13)
                        .color(style::SUBTLE),
                    text(confirmed_text(
                        &context.controller().confirmed_strategies(role)
                    ))
                    .size(12)
                    .color(style::SUBTLE),
                ]
                .width(Length::Fill),
            ]
            .spacing(8),
        );
    }
    body = body.push(section(tr("Services"))).push(services);

    // Technical details, folded.
    body = body.push(ui::link(
        if state.details {
            tr("Hide details")
        } else {
            tr("Details")
        },
        crate::Message::Network(Action::Details(!state.details)),
    ));
    if state.details {
        body = body.push(
            column![
                text(budget_text(context.controller().budget_snapshot())).size(13),
                text(search_text(context.controller().search_progress())).size(13),
                button(text(tr("Try all TLS strategies (bounded)")).size(13))
                    .on_press_maybe((context.mode() == Mode::Auto && !state.search_busy()).then_some(crate::Message::Network(Action::FullSearch)))
                    .padding([6,12]).style(style::secondary_button(false)),
                ui::note(tr("Uses only an eligible failed endpoint. Each click keeps the same probe and cooldown limits; an incomplete search continues later. No login or write is replayed.")),
                ui::note(tr(
                    "Auto checks only a required service after an eligible connection failure. TLS reachability does not establish that Discord or media works. Recovery is bounded; it never bypasses certificate verification."
                )),
                ui::note(tr(
                    "Recovery can split the TLS ClientHello into several records, and as a next step cut the last 30 bytes of the server name into one-byte records 30 ms apart, so that no write starts on a record boundary. The handshake bytes, certificate verification and ALPN are unchanged. It is still best effort: TCP writes do not guarantee packet boundaries, and UDP media has a separate connection path."
                )),
                ui::note(tr(
                    "Optional Windows helper requires UAC once per session. It handles public TLS handshakes and voice-server discovery for fastcord. The client and account stay unprivileged. Starting it selects Auto."
                )),
                ui::note(tr(
                    "Proxy mode never falls back to a direct connection. CONNECT proxies carry TCP only: voice, camera and screen sharing cannot join through them. SOCKS5 UDP requires a UDP-capable proxy and a working UDP route to that proxy. It does not carry media over TCP and does not encrypt the client-to-proxy transport. Passwords are saved in native credential storage."
                )),
                button(text(tr("Retry pending TLS check")).size(13))
                    .on_press(crate::Message::Network(Action::Retry))
                    .padding([6, 12])
                    .style(style::secondary_button(false)),
            ]
            .spacing(8),
        );
    }
    body.into()
}

/// The route a new dial takes, so a user who lost the connection can see which
/// one is in force. There is no byedpi path: that engine is not linked.
fn path_text(mode: Mode, packet: bool, proxy: Option<ProxyKind>) -> String {
    use crate::i18n::{tr, trf};
    match mode {
        Mode::Off => tr("Active path: direct, no recovery").into(),
        Mode::Auto if packet => {
            tr("Active path: direct, TLS recovery and the packet helper").into()
        }
        Mode::Auto => tr("Active path: direct with TLS recovery; no packet helper").into(),
        Mode::Proxy => trf(
            "Active path: proxy ({}), never direct",
            &[&proxy.unwrap_or(ProxyKind::Http)],
        ),
    }
}

fn confirmed_text(
    candidates: &[(
        fastcord_net::selector::TlsProfile,
        fastcord_net::tls::Strategy,
    )],
) -> String {
    if candidates.is_empty() {
        return tr("No TLS shape confirmed for this service").into();
    }
    // These are actual twice-probed candidates, not a guessed global winner
    // or a claim about every pre-existing HTTP pooled connection.
    let labels = candidates
        .iter()
        .take(3)
        .map(|(profile, strategy)| format!("{profile:?}: {strategy:?}"))
        .collect::<Vec<_>>()
        .join("; ");
    let extra = candidates.len().saturating_sub(3);
    if extra == 0 {
        format!("{}: {labels}", tr("Confirmed TLS candidates"))
    } else {
        format!("{}: {labels}; +{extra}", tr("Confirmed TLS candidates"))
    }
}
fn search_text(progress: Option<fastcord_net::selector::SearchProgress>) -> String {
    let Some(progress) = progress else {
        return tr("Extended search has not started; a failed required endpoint is needed.").into();
    };
    let state = if progress.confirmed {
        tr("TLS candidate confirmed; service success is separate")
    } else if progress.tested < progress.total {
        tr("incomplete; remaining strategies have not all been tested")
    } else {
        tr("catalog exhausted; no TLS candidate confirmed")
    };
    format!(
        "{:?}: {}/{} {}; {} {}s",
        progress.role,
        progress.tested,
        progress.total,
        state,
        tr("next campaign in at least"),
        progress
            .retry_after
            .as_secs()
            .saturating_add(u64::from(progress.retry_after.subsec_nanos() > 0))
    )
}

fn budget_text(budget: fastcord_net::selector::BudgetSnapshot) -> String {
    use crate::i18n::{tr, trf};
    use fastcord_net::selector::{MAX_ROLLING_DIALS, MAX_TLS_PROBES};
    let campaign = if budget.campaign_active {
        trf(
            "checking, {}/{} probes",
            &[&budget.campaign_probes, &MAX_TLS_PROBES],
        )
    } else {
        tr("idle").into()
    };
    let rest = if budget.rolling_dials + 2 > MAX_ROLLING_DIALS {
        tr(" Budget spent: new checks wait up to 10 minutes.")
    } else {
        ""
    };
    trf(
        "Recovery: {}; {}/{} dials used in the last 10 minutes.{}",
        &[&campaign, &budget.rolling_dials, &MAX_ROLLING_DIALS, &rest],
    )
}

pub(crate) fn status_text(status: fastcord_net::selector::ServiceStatus) -> String {
    use crate::i18n::{tr, trf};
    use fastcord_net::selector::ServiceStatus;
    match status {
        ServiceStatus::Unknown => tr("Not checked").into(),
        ServiceStatus::Selecting => tr("Checking TLS routes…").into(),
        ServiceStatus::TlsReachable => tr("TLS reachable; service not confirmed").into(),
        ServiceStatus::ServiceReachable => tr("Service answered").into(),
        ServiceStatus::SessionHealthy => tr("Session connected").into(),
        ServiceStatus::UdpUnreachable => tr("UDP media did not answer; TLS is separate").into(),
        ServiceStatus::UdpDiscovering { variant } => {
            trf("Experimental UDP discovery: variant {}/3", &[&variant])
        }
        ServiceStatus::Failed(fastcord_net::NetworkError::BudgetExhausted) => {
            tr("Recovery budget reached; some strategy checks may remain incomplete.").into()
        }
        ServiceStatus::Failed(error) => tr(&error.to_string()).to_owned(),
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

/// NetworkManager identity is separate from generic change hints. Fake mode
/// never reads the system bus or installation salt. Other platforms explicitly
/// stay process-only until they implement a genuine OS-profile identity.
pub fn profiles() -> impl iced::futures::Stream<Item = Option<String>> {
    iced::stream::channel(
        1,
        |mut output: iced::futures::channel::mpsc::Sender<Option<String>>| async move {
            use iced::futures::SinkExt;
            #[cfg(target_os = "linux")]
            if !crate::fake_mode()
                && let Some(directory) = crate::audio_devices::data_dir()
            {
                let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
                let task = tokio::spawn(fastcord_platform::network::profile::watch(
                    directory, sender,
                ));
                struct Abort(tokio::task::JoinHandle<()>);
                impl Drop for Abort {
                    fn drop(&mut self) {
                        self.0.abort();
                    }
                }
                let _guard = Abort(task);
                while let Some(profile) = receiver.recv().await {
                    if output.send(profile).await.is_err() {
                        return;
                    }
                }
            }
            let _ = output.send(None).await;
            std::future::pending::<()>().await;
        },
    )
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
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                eprintln!("Network check: runtime could not start.");
                return;
            };
            runtime.block_on(async {
                for mode in [Mode::Off, Mode::Auto] {
                    let context = new_context().with_mode(mode);
                    context.debug_probe_output(true);
                    // Exercise the same one-click flow as the Network page.
                    println!("{mode:?}: {}", packet_check(context.clone()).await);
                    println!(
                        "{mode:?}: service={:?}; budget={:?}",
                        context
                            .controller()
                            .service_status(fastcord_net::Role::Rest),
                        context.controller().budget_snapshot()
                    );
                }
            });
        });
    if !worker.is_ok_and(|worker| worker.join().is_ok()) {
        eprintln!("Network check: worker stopped.");
    }
}

/// UI check without credential access or account restoration. Network and UAC
/// actions require the same explicit buttons as the ordinary network screen.
#[cfg(debug_assertions)]
pub fn preview() -> iced::Result {
    struct Preview {
        state: State,
        context: fastcord_net::NetworkContext,
    }
    fn view(state: &Preview) -> Element<'_, crate::Message> {
        iced::widget::center(iced::widget::scrollable(
            container(super::network_ui::view(&state.state, &state.context, true))
                .padding(24)
                .width(650),
        ))
        .into()
    }
    crate::render::application(
        || Preview {
            state: State {
                open: true,
                notice: Some(
                    "Diagnostic preview: no account or credential access. Use the buttons to start an anonymous API check or the optional packet helper.",
                ),
                ..State::default()
            },
            context: fastcord_net::NetworkContext::off(),
        },
        |state: &mut Preview, message: crate::Message| {
            if let crate::Message::Network(action) = message {
                match action {
                    Action::Details(open) => state.state.details = open,
                    Action::PacketStart if !state.state.saving => {
                        state.state.saving = true;
                        state.state.notice = Some("Waiting for Windows administrator approval…");
                        return iced::Task::perform(start_packet(), |r| crate::Message::Network(Action::PacketStarted(r)));
                    }
                    Action::PacketStarted(result) => {
                        state.state.saving = false;
                        match result {
                            Ok(route) => {
                                state.state.packet_active = true;
                                state.state.settings.mode = Mode::Auto;
                                state.context = state.context.with_mode(Mode::Auto).with_prefix_factory(route.0);
                                state.state.saving = true;
                                state.state.notice = Some("Packet helper started. Checking the public Discord API…");
                                return iced::Task::perform(packet_check(state.context.clone()), |r| crate::Message::Network(Action::PacketChecked(r)));
                            }
                            Err(e) => state.state.notice = Some(e),
                        }
                    }
                    Action::PacketStop if !state.state.saving => {
                        state.state.packet_active = false;
                        state.context = state.context.without_prefix_factory();
                        state.state.notice = Some("Packet recovery stopped.");
                    }
                    Action::PacketCheck if !state.state.saving => {
                        state.state.saving = true;
                        state.state.notice = Some("Checking the public Discord API…");
                        return iced::Task::perform(packet_check(state.context.clone()), |r| crate::Message::Network(Action::PacketChecked(r)));
                    }
                    Action::PacketChecked(r) => { state.state.saving = false; state.state.notice = Some(r); }
                    Action::Mode(mode) => {
                        state.state.settings.mode = mode;
                        state.context = state.context.with_mode(mode);
                        if mode != Mode::Auto {
                            state.state.packet_active = false;
                            state.context = state.context.without_prefix_factory();
                        }
                    }
                    Action::ProxyKind(kind) => state.state.proxy_kind = Some(kind),
                    Action::ProxyHost(value) => state.state.proxy_host = value,
                    Action::ProxyPort(value) => state.state.proxy_port = value,
                    Action::ProxyUsername(value) => state.state.proxy_username = value,
                    Action::ProxyPassword(value) => state.state.proxy_password = Some(value),
                    _ => {}
                }
            }
            iced::Task::none()
        },
        view,
    )
    .title("fastcord — Network diagnostic")
    .theme(|_: &Preview| crate::style::theme())
    .window(iced::window::Settings {
        size: iced::Size::new(820.0, 800.0),
        icon: crate::icons::window_icon(),
        ..Default::default()
    })
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_net::selector::BudgetSnapshot;

    #[test]
    fn connection_check_is_platform_neutral_and_respects_busy_state() {
        use crate::variable_list::harness::Ui;
        let context = fastcord_net::NetworkContext::off();
        for native in [false, true] {
            for (editable, saving) in [(true, false), (false, false), (true, true)] {
                let state = State {
                    saving,
                    ..State::default()
                };
                let mut ui = Ui::new(
                    820.0,
                    1600.0,
                    view_with_capability(&state, &context, editable, native),
                );
                let mut check = false;
                let mut helper = false;
                for y in (0..700).step_by(8) {
                    for x in (0..820).step_by(8) {
                        for event in ui.click(x as f32, y as f32) {
                            match event {
                                crate::Message::Network(Action::PacketCheck) => check = true,
                                crate::Message::Network(Action::PacketStart) => helper = true,
                                _ => {}
                            }
                        }
                    }
                }
                assert_eq!(check, editable && !saving);
                assert_eq!(helper, native && editable && !saving);
            }
        }
    }

    #[test]
    fn stale_search_completion_cannot_clear_a_new_request_after_settings_reload() {
        let mut state = State::default();
        let old = state.begin_search(4);
        state = State::default();
        let current = state.begin_search(5);
        assert!(!state.finish_search(old, 5));
        assert!(state.search_busy());
        assert!(state.finish_search(current, 5));
        assert!(!state.search_busy());
        // Even a reload in the same network generation cannot reuse a ticket.
        let old = state.begin_search(5);
        state = State::default();
        let current = state.begin_search(5);
        assert!(!state.finish_search(old, 5));
        assert!(state.search_busy());
        assert!(state.finish_search(current, 5));
        let cancelled = state.begin_search(5);
        assert!(!state.finish_search(cancelled, 6));
        assert!(!state.search_busy());
    }

    #[test]
    fn source_address_is_numeric_session_only_and_invalid_edits_preserve_context() {
        let context = fastcord_net::NetworkContext::off();
        let mut state = State::default();
        for invalid in ["example.com", "127.0.0.1:8080", "0.0.0.0", "fe80::1%eth0"] {
            state.source_address = invalid.into();
            assert!(state.apply_source(&context).is_err());
            assert_eq!(context.source_address(), None);
        }
        state.source_address = "127.0.0.1".into();
        let selected = state.apply_source(&context).unwrap();
        assert_eq!(
            selected.source_address(),
            Some("127.0.0.1".parse().unwrap())
        );
        assert!(
            !serde_json::to_string(&state.settings)
                .unwrap()
                .contains("127.0.0.1")
        );
        state.source_address.clear();
        assert_eq!(
            state.apply_source(&selected).unwrap().source_address(),
            None
        );
    }

    #[test]
    fn full_search_text_distinguishes_incomplete_budget_from_all_failed() {
        let progress = fastcord_net::selector::SearchProgress {
            role: fastcord_net::Role::Rest,
            tested: 6,
            total: 42,
            confirmed: false,
            retry_after: std::time::Duration::from_secs(58),
        };
        let text = search_text(Some(progress));
        assert!(text.contains("6/42") && text.contains("incomplete") && text.contains("58s"));
        assert!(
            search_text(Some(fastcord_net::selector::SearchProgress {
                tested: 42,
                ..progress
            }))
            .contains("exhausted")
        );
        assert!(confirmed_text(&[]).contains("No TLS shape confirmed"));
        let candidates = [(
            fastcord_net::selector::TlsProfile::Http,
            fastcord_net::tls::Strategy::RecordFragment,
        )];
        let text = confirmed_text(&candidates);
        assert!(text.contains("candidate") && text.contains("RecordFragment"));
    }

    #[test]
    fn hint_saves_serialize_and_keep_original_confirmation_age() {
        let digest = "b".repeat(64);
        let timestamp = now() - 100;
        let mut source = Settings::default();
        source
            .remember_hint(
                &digest,
                fastcord_net::Role::Gateway,
                fastcord_net::tls::Strategy::SniFragment,
                timestamp,
            )
            .unwrap();
        let hints = source.profiles[0].hints.clone();
        let context = fastcord_net::NetworkContext::new(Mode::Auto);
        let mut state = State {
            profile_digest: Some(digest.clone()),
            ..State::default()
        };
        state.apply(&Loaded::default());
        state.bind_profile(&context);
        context.controller().load_profile_hints(0, &hints, now());
        let first = state.collect_hints(&context).unwrap();
        assert_eq!(first.profiles[0].hints[0].confirmed_at_unix_secs, timestamp);
        assert!(state.saving);
        context.controller().load_profile_hints(0, &[], now());
        assert!(
            state.collect_hints(&context).is_none(),
            "no overlapping write"
        );
        state.saving = false; // first save completion
        let second = state.collect_hints(&context).unwrap();
        assert!(
            second.profiles.is_empty(),
            "pending eviction must be saved next"
        );
    }

    #[test]
    fn profile_preferences_stay_out_of_proxy_and_off_modes() {
        let digest = "a".repeat(64);
        let mut settings = Settings::default();
        settings
            .remember_hint(
                &digest,
                fastcord_net::Role::Gateway,
                fastcord_net::tls::Strategy::SniFragment,
                now(),
            )
            .unwrap();
        let loaded = Loaded {
            settings,
            ..Loaded::default()
        };
        for mode in [Mode::Off, Mode::Proxy, Mode::Auto] {
            let mut state = State {
                profile_digest: Some(digest.clone()),
                ..State::default()
            };
            state.apply(&loaded);
            let context = fastcord_net::NetworkContext::new(mode);
            state.bind_profile(&context);
            assert_eq!(
                context.controller().profile_hints().2.len(),
                usize::from(mode == Mode::Auto)
            );
            assert!(
                state.collect_hints(&context).is_none(),
                "import does not rewrite settings or renew age"
            );
        }
    }

    #[test]
    fn a_failed_check_blames_only_the_route_it_took() {
        assert!(!check_failed(Mode::Off, false).contains("Packet"));
        assert!(check_failed(Mode::Auto, false).contains("not running"));
        assert!(check_failed(Mode::Auto, true).contains("Packet recovery"));
    }

    #[test]
    fn budget_line_says_when_checks_must_wait() {
        let budget = BudgetSnapshot {
            campaign_active: true,
            generation: 0,
            rolling_dials: 23,
            campaign_probes: 4,
            campaign_dials: 8,
            campaign_bytes: 0,
        };
        let line = budget_text(budget);
        assert!(line.contains("checking, 4/6 probes"), "{line}");
        assert!(line.contains("23/24 dials"), "{line}");
        assert!(line.contains("wait up to 10 minutes"), "{line}");
        let idle = budget_text(BudgetSnapshot {
            campaign_active: false,
            rolling_dials: 2,
            ..budget
        });
        assert!(idle.starts_with("Recovery: idle; 2/24"), "{idle}");
        assert!(!idle.contains("wait"), "{idle}");
    }
}
