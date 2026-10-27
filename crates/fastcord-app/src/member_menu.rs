//! Participant interaction and a bounded context menu. Opening it is local;
//! profile reads and server mutations require their own explicit action.
use crate::i18n::{tr, trf};
use crate::ui::button;
use crate::{Message, friends, gateway::VoiceControls, style};
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{MemberAction, RestError};
use fastcord_model::{Snowflake, User};
use iced::widget::{
    column, container, mouse_area, opaque, responsive, row, scrollable, slider, space, stack, text,
    text_input,
};
use iced::{Element, Length, Point};
use iced_renderer::core::{self, layout, mouse, overlay, renderer, widget};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum Event {
    Open(User, Point),
    Close,
    Select(MemberAction),
    Confirm,
    Done(u64, Result<(), RestError>),
    /// Opens or closes one of the choice lists below the actions.
    Sub(Option<Sub>),
    /// Optional audit-log reason, kept while the choice changes.
    Reason(String),
    Nick(String),
    /// The message history a ban deletes (a `BAN_DELETE` preset).
    BanWindow(u32),
}

/// The choice lists of the menu: each ends in one explicit action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sub {
    Timeout,
    Ban,
    Nick,
    Move,
}

#[derive(Debug)]
pub struct Target {
    pub user: User,
    pub point: Point,
    pub guild: Option<Snowflake>,
    pub confirm: Option<MemberAction>,
    pub sub: Option<Sub>,
    pub reason: String,
    pub nick: String,
    pub ban_window: u32,
}

#[derive(Debug, Default)]
pub struct State {
    pub target: Option<Target>,
    /// The running request and the member it is for.
    pub pending: Option<(u64, Snowflake)>,
    pub serial: u64,
    pub notice: Option<String>,
}

impl State {
    pub fn close(&mut self) {
        self.target = None;
        self.notice = None;
    }
    pub fn reset(&mut self) {
        self.close();
        self.pending = None;
        self.serial = self.serial.wrapping_add(1);
    }

    /// Starts a request for `user`; its result arrives as `Done(serial, ..)`.
    pub fn begin(&mut self, user: Snowflake) -> u64 {
        self.serial = self.serial.wrapping_add(1);
        self.pending = Some((self.serial, user));
        self.notice = None;
        self.serial
    }

    /// A request finished. Its text goes only to the menu of the member it was
    /// for: another member's open menu keeps its input and shows nothing.
    pub fn done(&mut self, serial: u64, text: String) {
        let Some((_, user)) = self.pending.filter(|(pending, _)| *pending == serial) else {
            return;
        };
        self.pending = None;
        if let Some(target) = self.target.as_mut().filter(|t| t.user.id == user) {
            target.confirm = None;
            target.sub = None;
            target.reason.clear();
            self.notice = Some(text);
        }
    }
}

fn action(
    label: impl Into<String>,
    message: Option<Message>,
    danger: bool,
) -> Element<'static, Message> {
    let label = label.into();
    button(text(tr(&label).to_owned()).size(13))
        .on_press_maybe(message)
        .width(Length::Fill)
        .style(style::tool_button(false, danger))
        .into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    voice: Arc<VoiceControls>,
    restricted: bool,
    volume_notice: Option<&'a str>,
) -> Option<Element<'a, Message>> {
    let target = state.target.as_ref()?;
    let user = target.user.id;
    let own = navigation
        .current_user
        .as_ref()
        .is_some_and(|me| me.id == user);
    let permissions = target
        .guild
        .filter(|id| Some(*id) == navigation.selected_guild)
        .and_then(|_| navigation.members.get(&user))
        .map(|m| m.moderation)
        .unwrap_or_default();
    Some(
        responsive(move |size| {
            let mut card = column![
                text(target.user.display_name().to_owned()).size(14),
                action(
                    "Profile",
                    Some(Message::Friends(friends::Event::Profile(
                        target.user.clone()
                    ))),
                    false
                ),
            ]
            .spacing(5);
            if let Some(guild) = target
                .guild
                .filter(|id| Some(*id) == navigation.selected_guild)
            {
                card = card.push(action(
                    "Roles\u{2026}",
                    Some(Message::GuildAdmin(crate::guild_admin::Event::MemberRoles(
                        guild, user,
                    ))),
                    false,
                ));
            }
            if !own {
                card = card.push(action(
                    "Message",
                    Some(Message::Friends(friends::Event::OpenDm(user))),
                    false,
                ));
                if voice.streams.iter().any(|offer| offer.owner == user) {
                    card = card.push(action(
                        "Watch stream",
                        Some(Message::ActivateParticipant(user)),
                        false,
                    ));
                }
                if voice.participants.iter().any(|p| p.user_id == user) {
                    let percent = voice.volume_percent(user);
                    card = card.push(text(trf("User volume · {}%", &[&percent])).size(12));
                    if let Some(notice) = volume_notice {
                        card = card.push(text(tr(notice).to_owned()).size(11));
                    }
                    card = card.push(
                        slider(0.0..=200.0, f32::from(percent), move |v| {
                            Message::SetParticipantVolume(user, v.round() as u16)
                        })
                        .on_release(Message::CommitParticipantVolume(user)),
                    );
                    card = card.push(action(
                        if percent == 0 {
                            "Unmute locally"
                        } else {
                            "Mute locally"
                        },
                        Some(Message::PresetParticipantVolume(
                            user,
                            if percent == 0 { 100 } else { 0 },
                        )),
                        false,
                    ));
                }
                if voice.stage.permissions.moderate
                    && let Some(seat) = voice.stage.seat(user)
                {
                    use fastcord_discord::StageAction;
                    use fastcord_discord::state::navigation::StageSeat;
                    let (label, request) = match seat {
                        StageSeat::Speaker => ("Move to audience", StageAction::Demote(user)),
                        StageSeat::Hand => ("Approve request", StageAction::Invite(user)),
                        StageSeat::Audience => ("Invite to speak", StageAction::Invite(user)),
                    };
                    card = card.push(text(tr("STAGE")).size(11).color(crate::style::muted()));
                    card = card.push(action(
                        label,
                        (!restricted).then_some(Message::Stage(crate::stage::Event::Do(request))),
                        false,
                    ));
                }
            }
            card = server_section(
                card,
                state,
                target,
                permissions,
                navigation,
                restricted,
                own,
            );
            if state.pending.is_some() {
                card = card.push(text(tr("Working…")).size(12));
            }
            if let Some(notice) = &state.notice {
                card = card.push(text(notice.as_str()).size(12));
            }
            card = card.push(action(
                "Close",
                Some(Message::MemberMenu(Event::Close)),
                false,
            ));
            let width = (size.width - 16.0).clamp(0.0, 270.0);
            let height = (size.height - 16.0).clamp(0.0, 620.0);
            let x = target
                .point
                .x
                .clamp(8.0_f32.min(size.width), (size.width - width - 8.0).max(8.0));
            let y = target.point.y.clamp(
                8.0_f32.min(size.height),
                (size.height - height - 8.0).max(8.0),
            );
            stack![
                mouse_area(space().width(Length::Fill).height(Length::Fill))
                    .on_press(Message::MemberMenu(Event::Close))
                    .on_right_press(Message::MemberMenu(Event::Close)),
                container(opaque(
                    container(scrollable(card).height(Length::Shrink))
                        .padding(12)
                        .width(width)
                        .max_height(height)
                        .style(style::card)
                ))
                .padding(iced::Padding::ZERO.left(x).top(y)),
            ]
            .into()
        })
        .into(),
    )
}

fn choice(
    label: impl Into<String>,
    on: bool,
    message: Option<Message>,
) -> Element<'static, Message> {
    let label = label.into();
    button(text(tr(&label).to_owned()).size(12))
        .on_press_maybe(message)
        .width(Length::Fill)
        .style(style::tool_button(on, false))
        .into()
}

/// The moderation actions of the menu: the nickname for the account itself,
/// and for another member voice, timeout, nickname, kick and ban. Every button
/// names one action; Discord is asked once, after the explicit Confirm where
/// there is one, and the reducer checks again before sending.
fn server_section<'a>(
    mut card: iced::widget::Column<'a, Message>,
    state: &'a State,
    target: &'a Target,
    permissions: fastcord_discord::MemberPermissions,
    navigation: &'a NavigationSnapshot,
    restricted: bool,
    own: bool,
) -> iced::widget::Column<'a, Message> {
    use fastcord_discord::{BAN_DELETE, TIMEOUTS};
    let idle = !restricted && state.pending.is_none();
    let send = |kind: MemberAction| idle.then_some(Message::MemberMenu(Event::Select(kind)));
    let open = |sub: Sub| Some(Message::MemberMenu(Event::Sub(Some(sub))));
    let name = target.user.display_name().to_owned();
    let mut rows = Vec::<Element<'a, Message>>::new();
    if own {
        if permissions.nickname {
            rows.push(action("Change nickname\u{2026}", open(Sub::Nick), false));
        }
    } else {
        if permissions.mute {
            let kind = MemberAction::Mute(!permissions.server_muted);
            rows.push(action(kind.label(), send(kind), true));
        }
        if permissions.deafen {
            let kind = MemberAction::Deafen(!permissions.server_deafened);
            rows.push(action(kind.label(), send(kind), true));
        }
        if permissions.disconnect {
            rows.push(action(
                MemberAction::Disconnect.label(),
                send(MemberAction::Disconnect),
                true,
            ));
            if !navigation.move_targets.is_empty() {
                rows.push(action("Move to\u{2026}", open(Sub::Move), false));
            }
        }
        if permissions.timeout {
            rows.push(action("Time out\u{2026}", open(Sub::Timeout), true));
        }
        if permissions.allows(&MemberAction::Timeout(None)) {
            let kind = MemberAction::Timeout(None);
            rows.push(action(kind.label(), send(kind), false));
        }
        if permissions.nickname {
            rows.push(action("Nickname\u{2026}", open(Sub::Nick), false));
        }
        if permissions.kick {
            rows.push(action(
                MemberAction::Kick.label(),
                send(MemberAction::Kick),
                true,
            ));
        }
        if permissions.ban {
            rows.push(action("Ban\u{2026}", open(Sub::Ban), true));
        }
    }
    if rows.is_empty() {
        return card;
    }
    card = card.push(
        text(if own {
            tr("YOUR NICKNAME")
        } else {
            tr("SERVER ACTIONS")
        })
        .size(11)
        .color(crate::style::muted()),
    );
    for element in rows {
        card = card.push(element);
    }
    let mut needs_reason = target
        .confirm
        .as_ref()
        .is_some_and(MemberAction::takes_reason);
    match target.sub {
        Some(Sub::Timeout) => {
            card = card.push(text(tr("Time out for")).size(12));
            for (seconds, label) in TIMEOUTS {
                card = card.push(choice(
                    label,
                    false,
                    send(MemberAction::Timeout(Some(seconds))),
                ));
            }
            needs_reason = true;
        }
        Some(Sub::Ban) => {
            card = card.push(text(tr("Delete messages from")).size(12));
            for (seconds, label) in BAN_DELETE {
                card = card.push(choice(
                    label,
                    target.ban_window == seconds,
                    Some(Message::MemberMenu(Event::BanWindow(seconds))),
                ));
            }
            card = card.push(action(
                "Ban",
                send(MemberAction::Ban(target.ban_window)),
                true,
            ));
            needs_reason = true;
        }
        Some(Sub::Nick) => {
            card = card.push(
                text_input(tr("Nickname (empty resets)"), &target.nick)
                    .on_input(|v| Message::MemberMenu(Event::Nick(v)))
                    .size(13),
            );
            card = card.push(
                row![
                    action(
                        "Save",
                        MemberAction::nick(own, &target.nick).and_then(send),
                        false
                    ),
                    action("Reset", MemberAction::nick(own, "").and_then(send), false),
                ]
                .spacing(6),
            );
            needs_reason = true;
        }
        Some(Sub::Move) => {
            card = card.push(text(tr("Move to voice channel")).size(12));
            for (channel, label) in navigation.move_targets.iter().take(24) {
                card = card.push(choice(
                    format!("\u{266A} {label}"),
                    false,
                    send(MemberAction::Move(*channel)),
                ));
            }
        }
        None => {}
    }
    if needs_reason {
        card = card.push(
            text_input(tr("Reason for the audit log (optional)"), &target.reason)
                .on_input(|v| Message::MemberMenu(Event::Reason(v)))
                .size(12),
        );
    }
    if let Some(confirm) = &target.confirm {
        let detail = match confirm {
            MemberAction::Ban(0) => tr(" Message history will be kept.").to_owned(),
            MemberAction::Ban(seconds) => BAN_DELETE
                .iter()
                .find(|(s, _)| s == seconds)
                .map_or_else(String::new, |(_, label)| {
                    trf(" Messages deleted: {}.", &[&tr(label).to_lowercase()])
                }),
            MemberAction::Timeout(Some(seconds)) => TIMEOUTS
                .iter()
                .find(|(s, _)| s == seconds)
                .map_or_else(String::new, |(_, label)| trf(" For {}.", &[&tr(label)])),
            _ => String::new(),
        };
        card =
            card.push(text(trf("{} for {}?{}", &[&tr(confirm.label()), &name, &detail])).size(13));
        card = card.push(
            row![
                action(
                    "Confirm",
                    idle.then_some(Message::MemberMenu(Event::Confirm)),
                    true
                ),
                action("Cancel", Some(Message::MemberMenu(Event::Close)), false)
            ]
            .spacing(6),
        );
    }
    card
}

/// Carries the actual click position without subscribing to pointer movement
/// or putting a stream of motion messages through the application's queue.
pub fn area<'a>(user: User, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Area {
        user,
        content: content.into(),
    })
}
struct Area<'a> {
    user: User,
    content: Element<'a, Message>,
}
impl core::Widget<Message, iced::Theme, iced::Renderer> for Area<'_> {
    fn size(&self) -> core::Size<Length> {
        self.content.as_widget().size()
    }
    fn children(&self) -> Vec<widget::Tree> {
        vec![widget::Tree::new(&self.content)]
    }
    fn diff(&self, tree: &mut widget::Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }
    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: core::Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &core::Event,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn core::Clipboard,
        shell: &mut core::Shell<'_, Message>,
        viewport: &core::Rectangle,
    ) {
        if matches!(
            event,
            core::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right))
        ) && cursor.is_over(layout.bounds())
            && cursor.is_over(*viewport)
            && let Some(point) = cursor.position()
        {
            shell.publish(Message::MemberMenu(Event::Open(self.user.clone(), point)));
            shell.capture_event();
            return;
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut iced::Renderer,
        theme: &iced::Theme,
        style: &renderer::Style,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &core::Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: core::Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &core::Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut widget::Tree,
        layout: core::Layout<'b>,
        renderer: &iced::Renderer,
        viewport: &core::Rectangle,
        translation: core::Vector,
    ) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(id: u64) -> Target {
        Target {
            user: User {
                id: Snowflake(id),
                username: format!("user{id}"),
                global_name: None,
                avatar: None,
                bot: false,
            },
            point: Point::ORIGIN,
            guild: None,
            confirm: None,
            sub: None,
            reason: format!("reason for {id}"),
            nick: String::new(),
            ban_window: 0,
        }
    }

    #[test]
    fn a_result_reaches_only_the_member_it_was_for() {
        let mut state = State {
            target: Some(target(1)),
            ..State::default()
        };
        let serial = state.begin(Snowflake(1));
        // Another member's menu opens while the request runs.
        state.target = Some(target(2));
        state.done(serial, "Done.".into());
        assert_eq!(state.pending, None);
        assert_eq!(state.notice, None, "B's menu must not say A's result");
        assert_eq!(state.target.as_ref().unwrap().reason, "reason for 2");

        let serial = state.begin(Snowflake(2));
        state.done(serial.wrapping_add(1), "stale".into());
        assert!(state.pending.is_some(), "a stale serial changes nothing");
        state.done(serial, "Done.".into());
        assert_eq!(state.notice.as_deref(), Some("Done."));
        assert!(state.target.as_ref().unwrap().reason.is_empty());
    }
}
