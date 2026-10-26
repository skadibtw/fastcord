//! The sign-in screen: one centered card with one thing to do at a time.
//! With saved logins it starts by choosing an account; otherwise (or after
//! "Sign in to another account") it shows one way to sign in, QR code or email
//! and password, with token login behind a quiet link. The risk notice is
//! short, its details one click away, and settings stay reachable before
//! signing in.

use crate::ui::button;
use fastcord_model::Snowflake;
use iced::widget::{
    checkbox, column, container, qr_code, row, scrollable, space, text, text_input,
};
use iced::{Alignment, Color, Element, Length};

use crate::i18n::{tr, trf};
use crate::icons::{self, Icon};
use crate::qr::QrStage;
use crate::{Message, style, ui};

pub const TOKEN_INPUT: &str = "token-input";
pub const LOGIN_INPUT: &str = "login-input";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Method {
    #[default]
    Qr,
    Password,
    Token,
}

#[derive(Clone, Debug)]
pub enum Event {
    /// Back to the saved accounts.
    Accounts,
    Method(Method),
    Choose(Snowflake),
    /// The `⋯` of a saved account, `None` to close it.
    AccountMenu(Option<Snowflake>),
    ShowPassword(bool),
    RiskDetails,
}

#[derive(Debug, Default)]
pub struct State {
    /// Showing the ways to sign in rather than the saved accounts.
    pub methods: bool,
    pub method: Method,
    pub choice: Option<Snowflake>,
    pub menu: Option<Snowflake>,
    pub show_password: bool,
    pub risk_details: bool,
}

impl State {
    /// The account "Continue" signs in: the one picked, else the first.
    pub fn chosen(&self, saved: &[Snowflake]) -> Option<Snowflake> {
        self.choice
            .filter(|id| saved.contains(id))
            .or_else(|| saved.first().copied())
    }

    pub fn update(&mut self, event: Event) -> iced::Task<Message> {
        self.menu = None;
        match event {
            Event::Accounts => self.methods = false,
            Event::Method(method) => {
                self.methods = true;
                self.method = method;
                return match method {
                    Method::Token => iced::widget::operation::focus(TOKEN_INPUT),
                    Method::Password => iced::widget::operation::focus(LOGIN_INPUT),
                    Method::Qr => iced::Task::none(),
                };
            }
            Event::Choose(id) => self.choice = Some(id),
            Event::AccountMenu(id) => self.menu = id,
            Event::ShowPassword(on) => self.show_password = on,
            Event::RiskDetails => self.risk_details = !self.risk_details,
        }
        iced::Task::none()
    }
}

/// What the screen is doing now.
pub enum Stage<'a> {
    Starting,
    Form,
    Qr(&'a QrStage),
    Mfa,
    Checking { restoring: bool },
    MemoryConsent { name: String, error: String },
    Deleting,
    DeleteFailed { error: String },
}

pub struct Inputs<'a> {
    pub stage: Stage<'a>,
    /// Saved accounts with their names.
    pub saved: Vec<(Snowflake, String)>,
    pub acknowledged: bool,
    pub memory_only: bool,
    pub token: &'a str,
    pub login: &'a str,
    pub password: &'a str,
    pub notice: Option<&'a str>,
    pub can_login: bool,
    pub can_password: bool,
    /// The credential store could not be opened.
    pub store_failed: bool,
}

fn brand<'a>() -> Element<'a, Message> {
    row![
        icons::sized(Icon::Brand, style::BLURPLE, 32.0),
        text("fastcord").size(24).font(ui::BOLD),
    ]
    .spacing(10)
    .align_y(Alignment::Center)
    .into()
}

fn titled<'a>(title: String, subtitle: Option<String>) -> Element<'a, Message> {
    let mut lines = column![text(title).size(24).font(ui::BOLD)]
        .spacing(4)
        .align_x(Alignment::Center);
    if let Some(subtitle) = subtitle {
        lines = lines.push(text(subtitle).size(14).color(style::SUBTLE));
    }
    container(lines).center_x(Length::Fill).into()
}

/// The page: the card in the middle, scrolling on a small window, settings
/// below it.
fn page<'a>(card: Element<'a, Message>) -> Element<'a, Message> {
    let settings = button(
        row![
            icons::sized(Icon::Settings, style::MUTED, 16.0),
            text(tr("Settings")).size(13).color(style::SUBTLE),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .on_press(Message::OpenAudioSettings)
    .padding([6, 10])
    .style(style::tool_button(false, false));
    container(scrollable(
        container(
            column![
                container(card)
                    .padding(32)
                    .width(Length::Fill)
                    .max_width(470)
                    .style(|_| container::Style {
                        background: Some(style::SIDEBAR.into()),
                        border: iced::Border {
                            radius: 10.0.into(),
                            ..iced::Border::default()
                        },
                        shadow: iced::Shadow {
                            color: Color::from_rgba(0.0, 0.0, 0.0, 0.35),
                            offset: iced::Vector::new(0.0, 8.0),
                            blur_radius: 30.0,
                        },
                        text_color: Some(style::TEXT),
                        ..container::Style::default()
                    }),
                settings,
            ]
            .spacing(14)
            .align_x(Alignment::Center),
        )
        .padding(24)
        .center_x(Length::Fill),
    ))
    .center(Length::Fill)
    .style(style::rail)
    .into()
}

pub fn view<'a>(state: &'a State, inputs: Inputs<'a>) -> Element<'a, Message> {
    let body = match &inputs.stage {
        Stage::Starting => waiting(tr("Looking for a saved login…")),
        Stage::Checking { restoring } => waiting(if *restoring {
            tr("Signing in…")
        } else {
            tr("Checking your login with Discord…")
        }),
        Stage::Deleting => waiting(tr("Removing the saved login from this device…")),
        Stage::DeleteFailed { error } => column![
            titled(tr("Could not remove the saved login").to_owned(), None),
            ui::error(error.clone()),
            ui::note(tr(
                "Account data was cleared, but the system credential store did not confirm the removal. Unlock the store and try again."
            )),
            ui::primary(tr("Try again"), Some(Message::RetryDelete)),
        ]
        .spacing(16)
        .into(),
        Stage::MemoryConsent { name, error } => column![
            titled(tr("Login could not be saved").to_owned(), Some(name.clone())),
            ui::error(error.clone()),
            ui::note(tr(
                "You can continue without saving: you will need to sign in again next time."
            )),
            ui::primary(tr("Continue without saving"), Some(Message::ContinueInMemory)),
            container(ui::link(tr("Cancel"), Message::CancelLogin)).center_x(Length::Fill),
        ]
        .spacing(16)
        .into(),
        Stage::Mfa => mfa(&inputs),
        Stage::Form | Stage::Qr(_) if !state.methods && !inputs.saved.is_empty() => {
            accounts(state, &inputs)
        }
        Stage::Form | Stage::Qr(_) => methods(state, &inputs),
    };
    page(
        column![
            container(brand()).center_x(Length::Fill),
            body,
            container(ui::link(
                tr("Network recovery"),
                Message::Network(crate::network_ui::Action::Open)
            ))
            .center_x(Length::Fill)
        ]
        .spacing(22)
        .into(),
    )
}

fn waiting<'a>(what: &'a str) -> Element<'a, Message> {
    container(text(what).size(15).color(style::SUBTLE))
        .center_x(Length::Fill)
        .padding([24, 0])
        .into()
}

fn accounts<'a>(state: &'a State, inputs: &Inputs<'a>) -> Element<'a, Message> {
    let ids: Vec<_> = inputs.saved.iter().map(|(id, _)| *id).collect();
    let chosen = state.chosen(&ids);
    let mut list = column![].spacing(2);
    for (id, name) in &inputs.saved {
        let selected = Some(*id) == chosen;
        let line = row![
            ui::initials(name, id.0, 36.0),
            text(name.clone())
                .size(15)
                .font(ui::BOLD)
                .width(Length::Fill),
        ]
        .spacing(12)
        .align_y(Alignment::Center);
        let more = icons::action(icons::Action {
            icon: Icon::More,
            on_press: Some(Message::LoginUi(Event::AccountMenu(
                (state.menu != Some(*id)).then_some(*id),
            ))),
            active: state.menu == Some(*id),
            danger: false,
            tip: tr("More").to_owned(),
            badge: 0,
        });
        list = list.push(
            row![
                button(line)
                    .on_press(Message::LoginUi(Event::Choose(*id)))
                    .width(Length::Fill)
                    .padding([8, 10])
                    .style(style::row_button(selected, selected)),
                more,
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        );
        if state.menu == Some(*id) {
            list = list.push(
                container(ui::menu_card(
                    ui::danger_item(
                        Some(Icon::Delete),
                        tr("Remove from this device"),
                        Some(Message::Forget(*id)),
                    ),
                    260.0,
                ))
                .align_right(Length::Fill),
            );
        }
    }
    let name = inputs
        .saved
        .iter()
        .find(|(id, _)| Some(*id) == chosen)
        .map_or_else(String::new, |(_, name)| name.clone());
    let mut body = column![
        titled(
            tr("Welcome back").to_owned(),
            Some(tr("Choose an account to sign in").to_owned())
        ),
        container(list).padding(6).style(|_| container::Style {
            background: Some(style::RAIL.into()),
            border: iced::Border {
                radius: 8.0.into(),
                ..iced::Border::default()
            },
            ..container::Style::default()
        }),
    ]
    .spacing(18);
    if let Some(notice) = inputs.notice {
        body = body.push(ui::error(notice));
        if inputs.store_failed {
            body = body.push(ui::neutral(
                tr("Retry saved login"),
                Some(Message::RetrySaved),
            ));
        }
    }
    body.push(ui::primary(
        trf("Continue as {}", &[&name]),
        chosen.map(Message::Restore),
    ))
    .push(
        container(ui::link(
            tr("Sign in to another account"),
            Message::LoginUi(Event::Method(Method::Qr)),
        ))
        .center_x(Length::Fill),
    )
    .into()
}

/// The short risk notice; its full text one click away. The checkbox is asked
/// once per sign-in, never for a saved account.
fn risk<'a>(state: &'a State, inputs: &Inputs<'a>) -> Element<'a, Message> {
    let mut notice = column![
        text(tr(
            "fastcord is an unofficial client. Use an additional account: Discord may block it."
        ))
        .size(13),
    ]
    .spacing(6);
    if state.risk_details {
        notice = notice.push(
            text(tr(
                "Third-party clients break Discord’s Terms of Service, and Discord has disabled accounts that used them. Never share your token or password with anyone: they give full access to the account."
            ))
            .size(13)
            .color(style::SUBTLE),
        );
    }
    notice = notice.push(ui::link(
        if state.risk_details {
            tr("Hide details")
        } else {
            tr("Learn more")
        },
        Message::LoginUi(Event::RiskDetails),
    ));
    column![
        ui::callout(notice),
        checkbox(inputs.acknowledged)
            .label(tr("I understand the risk and use an additional account"))
            .on_toggle(Message::Acknowledge)
            .size(16)
            .text_size(13),
    ]
    .spacing(10)
    .into()
}

fn methods<'a>(state: &'a State, inputs: &Inputs<'a>) -> Element<'a, Message> {
    let mut body = column![titled(tr("Sign in to fastcord").to_owned(), None)].spacing(18);
    if !inputs.saved.is_empty() {
        body = body.push(ui::link(
            tr("← Saved accounts"),
            Message::LoginUi(Event::Accounts),
        ));
    }
    body = body.push(risk(state, inputs));
    let qr = match &inputs.stage {
        Stage::Qr(stage) => Some(*stage),
        _ => None,
    };
    let method = if qr.is_some() {
        Method::Qr
    } else {
        state.method
    };
    if method != Method::Token {
        body = body.push(ui::tabs(
            &[
                (Method::Qr, tr("QR code")),
                (Method::Password, tr("Email and password")),
            ],
            method,
            |m| Message::LoginUi(Event::Method(m)),
        ));
    }
    body = body.push(match method {
        Method::Qr => qr_tab(qr, inputs),
        Method::Password => password_tab(state, inputs),
        Method::Token => token_tab(inputs),
    });
    body = body.push(
        checkbox(!inputs.memory_only)
            .label(tr("Keep me signed in on this device"))
            .on_toggle(|keep| Message::MemoryOnly(!keep))
            .size(16)
            .text_size(13),
    );
    body = body.push(ui::note(store_caption()));
    body = body.push(
        container(match method {
            Method::Token => ui::link(
                tr("Other ways to sign in"),
                Message::LoginUi(Event::Method(Method::Qr)),
            ),
            _ => ui::link(
                tr("Sign in with a token"),
                Message::LoginUi(Event::Method(Method::Token)),
            ),
        })
        .center_x(Length::Fill),
    );
    body.into()
}

/// Where the login is kept, for this operating system only.
fn store_caption() -> &'static str {
    if cfg!(windows) {
        tr("Login details are kept in the protected Windows Credential Manager.")
    } else if cfg!(target_os = "macos") {
        tr("Login details are kept in the macOS Keychain.")
    } else {
        tr("Login details are kept in the system Secret Service.")
    }
}

fn qr_tab<'a>(stage: Option<&'a QrStage>, inputs: &Inputs<'a>) -> Element<'a, Message> {
    let notice = || inputs.notice.map(ui::error);
    let Some(stage) = stage else {
        let mut body = column![
            ui::note(tr(
                "Open Discord on your phone, go to Settings, choose Scan QR Code and point the camera at the code."
            )),
        ]
        .spacing(12);
        if let Some(notice) = notice() {
            body = body.push(notice);
        }
        return body
            .push(ui::primary(
                tr("Show QR code"),
                inputs.acknowledged.then_some(Message::StartQr),
            ))
            .into();
    };
    let body = match stage {
        QrStage::Connecting => column![waiting(tr("Preparing a one-time code…"))],
        QrStage::Showing { code, expires_in } => column![
            container(
                container(qr_code(code).cell_size(6).style(|_| qr_code::Style {
                    // Scanners expect dark modules on a light field in any theme.
                    cell: Color::BLACK,
                    background: Color::WHITE,
                }))
                .padding(10)
                .style(|_| container::background(Color::WHITE)),
            )
            .center_x(Length::Fill),
            ui::note(tr(
                "On your phone: Discord → Settings → Scan QR Code, then confirm the sign-in."
            )),
            ui::note(trf(
                "The code expires in {}.",
                &[&crate::qr::describe_lifetime(*expires_in)]
            )),
            row![
                ui::neutral(tr("New code"), Some(Message::StartQr)),
                space::horizontal(),
                ui::link(tr("Cancel"), Message::CancelQr),
            ]
            .align_y(Alignment::Center),
        ],
        QrStage::Scanned(user) => column![
            text(trf("Scanned by @{}", &[&user.username]))
                .size(16)
                .font(ui::BOLD),
            ui::note(tr(
                "Confirm the sign-in on your phone. If this is not your account, cancel it there or here."
            )),
            ui::neutral(tr("Cancel"), Some(Message::CancelQr)),
        ],
        QrStage::Ended(error) => {
            let mut actions = row![].spacing(12).align_y(Alignment::Center);
            if error.can_regenerate() {
                actions = actions.push(ui::neutral(tr("New code"), Some(Message::StartQr)));
            }
            column![
                ui::error(tr(&error.to_string()).to_owned()),
                actions.push(ui::link(tr("Back"), Message::CancelQr)),
            ]
        }
    };
    body.spacing(12).into()
}

fn password_tab<'a>(state: &'a State, inputs: &Inputs<'a>) -> Element<'a, Message> {
    let submit = inputs.can_password.then_some(Message::PasswordSubmit);
    let mut body = column![
        column![
            ui::label(tr("EMAIL OR PHONE")),
            text_input("", inputs.login)
                .id(LOGIN_INPUT)
                .on_input(Message::PasswordLoginEdited)
                .padding(10),
        ]
        .spacing(6),
        column![
            ui::label(tr("PASSWORD")),
            row![
                text_input("", inputs.password)
                    .secure(!state.show_password)
                    .on_input(crate::password_edited)
                    .on_submit_maybe(submit.clone())
                    .padding(10),
                ui::link(
                    if state.show_password {
                        tr("Hide")
                    } else {
                        tr("Show")
                    },
                    Message::LoginUi(Event::ShowPassword(!state.show_password)),
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(6),
    ]
    .spacing(14);
    if let Some(notice) = inputs.notice {
        body = body.push(ui::error(notice));
    }
    body.push(ui::primary(tr("Sign in"), submit)).into()
}

fn token_tab<'a>(inputs: &Inputs<'a>) -> Element<'a, Message> {
    let submit = inputs.can_login.then_some(Message::Login);
    let mut body = column![
        column![
            ui::label(tr("TOKEN")),
            text_input("", inputs.token)
                .id(TOKEN_INPUT)
                .secure(true)
                .on_input(crate::token_edited)
                .on_submit_maybe(submit.clone())
                .padding(10),
            ui::note(tr(
                "Never share your token: it gives full access to the account."
            )),
        ]
        .spacing(6),
    ]
    .spacing(14);
    if let Some(notice) = inputs.notice {
        body = body.push(ui::error(notice));
    }
    body.push(ui::primary(tr("Sign in with token"), submit))
        .into()
}

fn mfa<'a>(inputs: &Inputs<'a>) -> Element<'a, Message> {
    let mut body = column![
        titled(
            tr("Two-factor check").to_owned(),
            Some(tr("Enter the code from your authenticator app").to_owned())
        ),
        column![
            ui::label(tr("CODE")),
            text_input("", inputs.token)
                .id(TOKEN_INPUT)
                .secure(true)
                .on_input(crate::token_edited)
                .on_submit(Message::MfaSubmit)
                .padding(10),
        ]
        .spacing(6),
    ]
    .spacing(16);
    if let Some(notice) = inputs.notice {
        body = body.push(ui::error(notice));
    }
    body.push(ui::primary(tr("Continue"), Some(Message::MfaSubmit)))
        .push(container(ui::link(tr("Cancel"), Message::CancelMfa)).center_x(Length::Fill))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_signs_in_the_picked_account_or_the_first() {
        let saved = [Snowflake(1), Snowflake(2)];
        let mut state = State::default();
        assert_eq!(state.chosen(&saved), Some(Snowflake(1)));
        let _ = state.update(Event::Choose(Snowflake(2)));
        assert_eq!(state.chosen(&saved), Some(Snowflake(2)));
        // A removed account falls back to the first one left.
        assert_eq!(state.chosen(&[Snowflake(1)]), Some(Snowflake(1)));
        assert_eq!(state.chosen(&[]), None);
    }
}
