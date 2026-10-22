//! Participant interaction and a bounded context menu. Opening it is local;
//! profile reads and server mutations require their own explicit action.
use crate::{Message, friends, gateway::VoiceControls, style};
use fastcord_discord::state::navigation::NavigationSnapshot;
use fastcord_discord::{MemberAction, RestError};
use fastcord_model::{Snowflake, User};
use iced::widget::{
    button, column, container, mouse_area, opaque, responsive, row, scrollable, slider, space,
    stack, text,
};
use iced::{Element, Length, Point};
use iced_renderer::core::{self, layout, mouse, overlay, renderer, widget};

#[derive(Clone, Debug)]
pub enum Event {
    Open(User, Point),
    Close,
    Select(MemberAction),
    Confirm,
    Done(u64, Result<(), RestError>),
}

#[derive(Debug)]
pub struct Target {
    pub user: User,
    pub point: Point,
    pub guild: Option<Snowflake>,
    pub confirm: Option<MemberAction>,
}

#[derive(Debug, Default)]
pub struct State {
    pub target: Option<Target>,
    pub pending: Option<u64>,
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
}

fn action(
    label: impl Into<String>,
    message: Option<Message>,
    danger: bool,
) -> Element<'static, Message> {
    button(text(label.into()).size(13))
        .on_press_maybe(message)
        .width(Length::Fill)
        .style(style::tool_button(false, danger))
        .into()
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
    voice: VoiceControls,
    restricted: bool,
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
                    card = card.push(text(format!("User volume · {percent}%")).size(12));
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
                    card = card.push(text("STAGE").size(11).style(text::secondary));
                    card = card.push(action(
                        label,
                        (!restricted).then_some(Message::Stage(crate::stage::Event::Do(request))),
                        false,
                    ));
                }
                let mut any = false;
                for (allowed, action_kind) in [
                    (
                        permissions.mute,
                        MemberAction::Mute(!permissions.server_muted),
                    ),
                    (
                        permissions.deafen,
                        MemberAction::Deafen(!permissions.server_deafened),
                    ),
                    (permissions.disconnect, MemberAction::Disconnect),
                    (permissions.kick, MemberAction::Kick),
                    (permissions.ban, MemberAction::Ban),
                ] {
                    if allowed {
                        if !any {
                            card =
                                card.push(text("SERVER ACTIONS").size(11).style(text::secondary));
                            any = true;
                        }
                        card = card.push(action(
                            action_kind.label(),
                            (!restricted && state.pending.is_none())
                                .then_some(Message::MemberMenu(Event::Select(action_kind))),
                            true,
                        ));
                    }
                }
                if let Some(confirm) = target.confirm {
                    card = card.push(
                        text(format!(
                            "{} for {}?",
                            confirm.label(),
                            target.user.display_name()
                        ))
                        .size(13),
                    );
                    if confirm == MemberAction::Ban {
                        card = card.push(text("Message history will be kept.").size(11));
                    }
                    card = card.push(
                        row![
                            action(
                                "Confirm",
                                (!restricted && state.pending.is_none())
                                    .then_some(Message::MemberMenu(Event::Confirm)),
                                true
                            ),
                            action("Cancel", Some(Message::MemberMenu(Event::Close)), false)
                        ]
                        .spacing(6),
                    );
                }
            }
            if state.pending.is_some() {
                card = card.push(text("Working…").size(12));
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
