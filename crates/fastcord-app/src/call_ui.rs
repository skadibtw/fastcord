//! The call's video tiles on screen (SPEC §8.10, issue #86): the compact strip
//! in the voice bar and the large call view with its grid, focus and speaker
//! layouts, pinning, full view and the people who have no video.
//!
//! Drawing only: which tiles exist and which are drawn is decided by
//! [`CallView::plan`], and a tile that is not drawn here is not decoded.

use fastcord_model::Snowflake;
use iced::widget::{button, checkbox, column, container, pick_list, row, scrollable, text};
use iced::{Element, Length};

use crate::Message;
use crate::call_layout::{CallView, Mode, Plan, Tile, TileId, TileSize, stream_owner};
use crate::gateway::{VoiceControls, preview_caption};
use crate::{playback, style};

/// People without video listed beside the tiles.
const MAX_LISTED_PEOPLE: usize = 24;

/// The users speaking now.
pub fn speaking(voice: &VoiceControls) -> Vec<Snowflake> {
    voice
        .participants
        .iter()
        .filter(|participant| participant.speaking)
        .map(|participant| participant.user_id)
        .collect()
}

fn label(tile: &Tile, voice: &VoiceControls) -> String {
    match &tile.id {
        TileId::Preview => preview_caption(&voice.camera),
        TileId::Camera(user) if tile.session.failed() => {
            format!(
                "{} · {}",
                voice.display_name(*user),
                tile.session.description()
            )
        }
        TileId::Camera(user) => voice.display_name(*user),
        TileId::Stream(key) => {
            let owner = stream_owner(key).map_or_else(
                || "A stream".to_owned(),
                |owner| format!("{}'s screen", voice.display_name(owner)),
            );
            format!("{owner} · {}", tile.session.description())
        }
    }
}

/// One tile: the picture, its caption and its buttons. Small tiles keep the
/// buttons because focusing is how a small tile becomes large.
fn tile<'a>(
    tile: &Tile,
    voice: &VoiceControls,
    view: &CallView,
    size: TileSize,
) -> Element<'a, Message> {
    let id = tile.id.clone();
    let speaking = matches!(&id, TileId::Camera(user) if self::speaking(voice).contains(user));
    let pinned = view.pinned.as_ref() == Some(&id);
    let full = view.full.as_ref() == Some(&id);
    let text_size = if size == TileSize::Large { 12 } else { 11 };
    let mut controls = row![text(label(tile, voice)).size(text_size).width(Length::Fill)]
        .spacing(6)
        .align_y(iced::alignment::Vertical::Center);
    controls = controls.push(
        button(text(if pinned { "Unpin" } else { "Pin" }).size(11))
            .on_press(Message::PinTile(id.clone()))
            .style(style::tool_button(pinned, false)),
    );
    controls = controls.push(
        button(text(if full { "Exit full view" } else { "Full view" }).size(11))
            .on_press(Message::FullTile((!full).then(|| id.clone())))
            .style(style::tool_button(full, false)),
    );
    if let TileId::Stream(key) = &id {
        controls = controls.push(
            button(text("Stop watching").size(11))
                .on_press(Message::UnwatchStream(key.clone()))
                .style(style::action_button(true)),
        );
    }
    container(
        column![
            container(playback::view::<Message>(tile.session.clone()))
                .width(Length::Fill)
                .height(Length::Fill),
            controls
        ]
        .spacing(3),
    )
    .padding(3)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(style::speaking(speaking))
    .into()
}

/// The compact strip of the voice bar: cameras only, all small, each opening
/// the call view on that tile. `None` while the call view is open (it draws
/// them) or when there is nothing to draw.
pub fn strip<'a>(
    view: &CallView,
    tiles: &[Tile],
    plan: &Plan,
    voice: &VoiceControls,
) -> Option<Element<'a, Message>> {
    if view.open || plan.shown.is_empty() {
        return None;
    }
    let mut strip = row![].spacing(8);
    for (id, _) in &plan.shown {
        let Some(found) = tiles.iter().find(|tile| tile.id == *id) else {
            continue;
        };
        strip = strip.push(
            column![
                container(playback::view::<Message>(found.session.clone()))
                    .width(Length::Fixed(192.0))
                    .height(Length::Fixed(108.0)),
                button(text(label(found, voice)).size(11))
                    .width(Length::Fixed(192.0))
                    .on_press(Message::OpenTile(id.clone()))
                    .style(style::plain_button),
            ]
            .spacing(2),
        );
    }
    let mut content = column![
        scrollable(strip).direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::default()
        ))
    ]
    .spacing(4);
    if let Some(refusal) = plan.refusal() {
        content = content.push(text(refusal).size(11).style(text::danger));
    }
    Some(content.into())
}

fn grid<'a>(
    plan: &Plan,
    tiles: &[Tile],
    voice: &VoiceControls,
    view: &CallView,
) -> Element<'a, Message> {
    let count = plan.shown.len();
    let columns = (count as f32).sqrt().ceil().max(1.0) as usize;
    let mut rows = column![].spacing(6).height(Length::Fill);
    for chunk in plan.shown.chunks(columns) {
        let mut line = row![].spacing(6).height(Length::Fill);
        for (id, size) in chunk {
            if let Some(found) = tiles.iter().find(|tile| tile.id == *id) {
                line = line.push(tile(found, voice, view, *size));
            }
        }
        // Keep the last row's tiles the same width as the others.
        for _ in chunk.len()..columns {
            line = line.push(container(text("")).width(Length::Fill));
        }
        rows = rows.push(line);
    }
    rows.into()
}

fn focus<'a>(
    plan: &Plan,
    tiles: &[Tile],
    voice: &VoiceControls,
    view: &CallView,
) -> Element<'a, Message> {
    let mut layout = column![].spacing(6).height(Length::Fill);
    let mut strip = row![].spacing(6).height(Length::Fill);
    let mut small = 0;
    for (id, size) in &plan.shown {
        let Some(found) = tiles.iter().find(|tile| tile.id == *id) else {
            continue;
        };
        if Some(id) == plan.main.as_ref() {
            layout = layout
                .push(container(tile(found, voice, view, *size)).height(Length::FillPortion(4)));
        } else {
            small += 1;
            strip = strip.push(tile(found, voice, view, *size));
        }
    }
    if small > 0 {
        layout = layout.push(container(strip).height(Length::FillPortion(1)));
    }
    layout.into()
}

/// The large call view that replaces the conversation while it is open.
pub fn panel<'a>(
    view: &'a CallView,
    tiles: &[Tile],
    plan: &Plan,
    voice: &VoiceControls,
    window_fullscreen: bool,
    notice: Option<&str>,
) -> Element<'a, Message> {
    let header = row![
        text("Call video").size(18).width(Length::Fill),
        pick_list(Mode::ALL, Some(view.mode), Message::SetCallMode).text_size(12),
        checkbox(!view.hide_people)
            .label("Show people without video")
            .size(14)
            .text_size(12)
            .on_toggle(|show| Message::SetHidePeople(!show)),
        button(
            text(if window_fullscreen {
                "Exit window full screen"
            } else {
                "Window full screen"
            })
            .size(12)
        )
        .on_press(Message::SetWindowFullscreen(!window_fullscreen))
        .style(style::tool_button(window_fullscreen, false)),
        button(text("Back to messages").size(12)).on_press(Message::ShowStream(false)),
    ]
    .spacing(10)
    .align_y(iced::alignment::Vertical::Center);
    let mut content = column![header].spacing(8);
    if let Some(notice) = notice.filter(|notice| !notice.is_empty()) {
        content = content.push(text(notice.to_owned()).size(12));
    }
    if let Some(refusal) = plan.refusal() {
        content = content.push(text(refusal).size(12).style(text::danger));
    }
    let body: Element<'a, Message> = if plan.shown.is_empty() {
        container(text("Nobody has video on. Cameras and watched streams appear here.").size(13))
            .center(Length::Fill)
            .into()
    } else if plan.main.is_some() {
        focus(plan, tiles, voice, view)
    } else {
        grid(plan, tiles, voice, view)
    };
    content = content.push(body);
    if !view.hide_people {
        let mut people = row![].spacing(8).align_y(iced::alignment::Vertical::Center);
        let mut listed = 0;
        let mut more = 0;
        for participant in &voice.participants {
            if tiles
                .iter()
                .any(|tile| tile.id == TileId::Camera(participant.user_id))
            {
                continue;
            }
            if listed == MAX_LISTED_PEOPLE {
                more += 1;
                continue;
            }
            listed += 1;
            let name = voice.display_name(participant.user_id);
            people = people.push(
                container(
                    row![
                        container(text(style::initial(&name)).size(12))
                            .center(24)
                            .style(style::avatar(style::avatar_color(participant.user_id.0))),
                        text(name).size(12),
                    ]
                    .spacing(6)
                    .align_y(iced::alignment::Vertical::Center),
                )
                .padding([3, 8])
                .style(style::speaking(participant.speaking)),
            );
        }
        if more > 0 {
            people = people.push(text(format!("and {more} more")).size(12));
        }
        if listed > 0 {
            content = content.push(scrollable(people).direction(
                scrollable::Direction::Horizontal(scrollable::Scrollbar::default()),
            ));
        }
    }
    container(content)
        .padding(16)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(iced::Color::from_rgb8(24, 25, 30).into()),
            ..Default::default()
        })
        .into()
}
