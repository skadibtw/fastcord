//! The call on screen (SPEC §8.10, issues #86 and #115): the voice panel at the
//! bottom of the sidebar with its call settings, and the large call view with
//! its grid, focus and speaker layouts, pinning, full view and the people who
//! have no video.
//!
//! Drawing only: which tiles exist and which are drawn is decided by
//! [`CallView::plan`], and a tile that is not drawn here is not decoded.

use fastcord_model::Snowflake;
use fastcord_video::effect::Effect;
use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, slider, text, tooltip,
};
use iced::{Element, Length};

use crate::Message;
use crate::call_layout::{CallView, Mode, Plan, Tile, TileId, TileSize, stream_owner};
use crate::gateway::{CameraChoice, VoiceAudioState, VoiceControls, preview_caption};
use crate::i18n::{tr, trf};
use crate::icons::{self, Action, Icon};
use crate::{desktop, playback, style};

const BOLD: iced::Font = iced::Font {
    weight: iced::font::Weight::Bold,
    ..iced::Font::DEFAULT
};

/// Names clip rather than wrap, so nothing grows past the 240 px sidebar.
const NAME: text::Wrapping = text::Wrapping::None;

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
                || tr("A stream").to_owned(),
                |owner| trf("{}'s screen", &[&voice.display_name(owner)]),
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
    controls = controls.push(icons::tool(
        if pinned { Icon::Unpin } else { Icon::Pin },
        if pinned { tr("Unpin") } else { tr("Pin") },
        Message::PinTile(id.clone()),
    ));
    controls = controls.push(icons::tool(
        if full {
            Icon::Restore
        } else {
            Icon::Fullscreen
        },
        if full {
            tr("Exit full view")
        } else {
            tr("Full view")
        },
        Message::FullTile((!full).then(|| id.clone())),
    ));
    if let TileId::Stream(key) = &id {
        if tile.session.failed() {
            controls = controls.push(
                button(text(tr("Retry watching")).size(11))
                    .on_press(Message::WatchStream(key.clone()))
                    .style(style::action_button(false)),
            );
        }
        controls = controls.push(
            button(icons::labeled(Icon::EyeOff, tr("Stop watching"), 11))
                .on_press(Message::UnwatchStream(key.clone()))
                .style(style::action_button(true)),
        );
    }
    let mut content = column![
        container(playback::view::<Message>(tile.session.clone()))
            .width(Length::Fill)
            .height(Length::Fill),
        controls
    ]
    .spacing(3);
    if let TileId::Stream(key) = &id {
        content = content.push(stream_audio_controls(key, &tile.session));
    }
    let content = container(content)
        .padding(3)
        .width(Length::Fill)
        .height(Length::Fill)
        .style(style::speaking(speaking))
        .into();
    match &id {
        TileId::Camera(user) => crate::member_menu::area(voice_user(voice, *user), content),
        TileId::Stream(key) => match stream_owner(key) {
            Some(user) => crate::member_menu::area(voice_user(voice, user), content),
            None => content,
        },
        TileId::Preview => content,
    }
}

fn voice_user(voice: &VoiceControls, user: Snowflake) -> fastcord_model::User {
    fastcord_model::User {
        id: user,
        username: voice.display_name(user),
        global_name: None,
        avatar: voice
            .avatars
            .iter()
            .find(|(id, _)| *id == user)
            .map(|(_, hash)| hash.clone()),
        bot: false,
    }
}

fn stream_audio_controls<'a>(key: &str, session: &playback::Session) -> Element<'a, Message> {
    let audio = session.audio_settings();
    let mute_key = key.to_owned();
    let volume_key = key.to_owned();
    column![
        row![
            button(
                text(if audio.muted {
                    tr("Unmute stream")
                } else {
                    tr("Mute stream")
                })
                .size(11)
            )
            .on_press(Message::MuteStream(mute_key, !audio.muted))
            .style(style::tool_button(audio.muted, false)),
            slider(0.0..=200.0, f32::from(audio.percent), move |value| {
                Message::SetStreamVolume(volume_key.clone(), value.round() as u16)
            })
            .width(Length::Fill),
            text(format!("{}%", audio.percent)).size(11),
        ]
        .width(Length::Fill)
        .spacing(6)
        .align_y(iced::alignment::Vertical::Center),
        text(session.audio_description())
            .size(11)
            .style(text::secondary),
    ]
    .width(Length::Fill)
    .spacing(3)
    .into()
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
        text(tr("Call video")).size(18).width(Length::Fill),
        pick_list(Mode::ALL, Some(view.mode), Message::SetCallMode).text_size(12),
        checkbox(!view.hide_people)
            .label(tr("Show people without video"))
            .size(14)
            .text_size(12)
            .on_toggle(|show| Message::SetHidePeople(!show)),
        button(
            text(if window_fullscreen {
                tr("Exit window full screen")
            } else {
                tr("Window full screen")
            })
            .size(12)
        )
        .on_press(Message::SetWindowFullscreen(!window_fullscreen))
        .style(style::tool_button(window_fullscreen, false)),
        button(icons::labeled(Icon::Chat, tr("Back to messages"), 12))
            .on_press(Message::ShowStream(false)),
    ]
    .spacing(10)
    .align_y(iced::alignment::Vertical::Center);
    let mut content = column![header].spacing(8);
    if let Some(notice) = notice.filter(|notice| !notice.is_empty()) {
        content = content.push(text(notice.to_owned()).size(12));
    }
    if let Some(note) = activity_note(voice) {
        content = content.push(text(note).size(12));
    }
    if let Some(refusal) = plan.refusal() {
        content = content.push(text(refusal).size(12).style(text::danger));
    }
    let body: Element<'a, Message> = if plan.shown.is_empty() {
        container(
            text(tr(
                "Nobody has video on. Cameras and watched streams appear here.",
            ))
            .size(13),
        )
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
            people = people.push(crate::member_menu::area(
                voice_user(voice, participant.user_id),
                button(
                    container(
                        row![
                            container(text(style::initial(&name)).size(12))
                                .center(24)
                                .style(style::avatar(style::avatar_color(participant.user_id.0))),
                            text(name).size(12),
                            text(
                                if voice.streams.iter().any(|s| s.owner == participant.user_id) {
                                    tr("LIVE")
                                } else {
                                    ""
                                }
                            )
                            .size(10)
                            .color(style::RED),
                        ]
                        .spacing(6)
                        .align_y(iced::alignment::Vertical::Center),
                    )
                    .padding([3, 8])
                    .style(style::speaking(participant.speaking)),
                )
                .on_press(Message::ActivateParticipant(participant.user_id))
                .style(style::plain_button),
            ));
        }
        if more > 0 {
            people = people.push(text(trf("and {} more", &[&more])).size(12));
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

/// The call view's line when an embedded Activity runs in the call's channel.
/// There is no join or launch: Activities need the official client.
pub fn activity_note(voice: &VoiceControls) -> Option<String> {
    if voice.activities.is_empty() {
        return None;
    }
    let names: Vec<&str> = voice
        .activities
        .iter()
        .map(String::as_str)
        .filter(|name| !name.is_empty())
        .collect();
    let mut note = tr("Running an Activity — join it in the official Discord client").to_owned();
    if !names.is_empty() {
        note = format!("{note} ({})", names.join(", "));
    }
    Some(note)
}

/// What the voice panel shows. Each button is `Some((state, enabled))` only
/// where the control exists on this platform and for this call.
pub struct Bar {
    pub state: VoiceAudioState,
    /// `channel / server`, or the name of the private call.
    pub place: String,
    /// Camera: (on, enabled). `None` where the camera cannot be sent.
    pub camera: Option<(bool, bool)>,
    /// Screen share: (sharing, enabled). `None` where capture is unsupported.
    pub share: Option<(bool, bool)>,
    /// Soundboard: (panel open, the message that toggles it). `None` outside
    /// a server call whose channel is known.
    pub soundboard: Option<(bool, Message)>,
    pub settings_open: bool,
    /// The call can be left.
    pub can_leave: bool,
    /// Progress or the last failure of the screen share.
    pub share_text: String,
}

/// The connected panel above the account footer: state and place (opening the
/// call view), Disconnect, and one row of icon buttons. Fits 240 px.
pub fn bar<'a>(bar: Bar) -> Element<'a, Message> {
    let (title, color, tip) = match bar.state {
        VoiceAudioState::Connecting => (
            tr("Connecting\u{2026}"),
            style::YELLOW,
            tr("Connecting to voice. Click to open the call"),
        ),
        VoiceAudioState::EngineFailed => (
            tr("Audio failed"),
            style::RED,
            tr("The audio devices could not be opened. Click to open the call"),
        ),
        _ => (
            tr("Voice connected"),
            style::GREEN,
            tr("Connected. Click to open the call"),
        ),
    };
    let place = if bar.place.is_empty() {
        tr("Call").to_owned()
    } else {
        bar.place
    };
    let heading = tooltip(
        button(
            row![
                icons::icon(Icon::Signal, color),
                column![
                    text(title).size(13).font(BOLD).color(color).wrapping(NAME),
                    text(place).size(11).style(text::secondary).wrapping(NAME),
                ]
                .width(Length::Fill),
            ]
            .spacing(6)
            .align_y(iced::alignment::Vertical::Center),
        )
        .on_press(Message::OpenCall)
        .width(Length::Fill)
        .padding(0)
        .style(style::plain_button),
        container(text(tip).size(11))
            .padding([3, 6])
            .style(container::rounded_box),
        tooltip::Position::Top,
    );
    let hang_up = icons::action(Action {
        icon: Icon::HangUp,
        on_press: bar.can_leave.then_some(Message::LeaveVoice),
        active: true,
        danger: true,
        tip: tr("Disconnect").to_owned(),
        badge: 0,
    });
    let mut buttons = row![].spacing(4);
    if let Some((on, enabled)) = bar.camera {
        buttons = buttons.push(icons::action(Action {
            icon: Icon::Camera { on },
            on_press: enabled.then_some(Message::ToggleCamera),
            active: on,
            danger: false,
            tip: if on {
                tr("Turn off camera")
            } else {
                tr("Turn on camera")
            }
            .to_owned(),
            badge: 0,
        }));
    }
    if let Some((sharing, enabled)) = bar.share {
        buttons = buttons.push(icons::action(Action {
            icon: Icon::Screen { sharing },
            on_press: enabled.then_some(if sharing {
                Message::StopShare
            } else {
                Message::OpenSharePicker
            }),
            active: sharing,
            danger: false,
            tip: if sharing {
                tr("Stop sharing")
            } else {
                tr("Share your screen")
            }
            .to_owned(),
            badge: 0,
        }));
    }
    if let Some((open, press)) = bar.soundboard {
        buttons = buttons.push(icons::action(Action {
            icon: Icon::Soundboard,
            on_press: Some(press),
            active: open,
            danger: false,
            tip: tr("Soundboard").to_owned(),
            badge: 0,
        }));
    }
    buttons = buttons.push(icons::action(Action {
        icon: Icon::Pin,
        on_press: Some(Message::ToggleCompact),
        active: false,
        danger: false,
        tip: tr(desktop::compact_tip()).to_owned(),
        badge: 0,
    }));
    buttons = buttons.push(icons::action(Action {
        icon: Icon::Settings,
        on_press: Some(Message::ToggleVoiceDetails),
        active: bar.settings_open,
        danger: false,
        tip: tr("Call settings: devices, camera, volume, streams").to_owned(),
        badge: 0,
    }));
    let mut content = column![
        row![heading, hang_up]
            .spacing(4)
            .align_y(iced::alignment::Vertical::Center),
        buttons,
    ]
    .spacing(6);
    // Sharing is always visible while it lasts. The status carries the health
    // line (fps, audio packets, copy-path reason), so it wraps inside the
    // footer instead of being clipped by it.
    if !bar.share_text.is_empty() {
        content = content.push(
            row![
                container(text(tr("LIVE")).size(9).font(BOLD))
                    .padding([1, 4])
                    .style(style::live_badge),
                text(tr(&bar.share_text).to_owned())
                    .size(11)
                    .wrapping(text::Wrapping::WordOrGlyph)
                    .width(Length::Fill),
            ]
            .spacing(6)
            .width(Length::Fill)
            .align_y(iced::alignment::Vertical::Center),
        );
    }
    container(content)
        .padding([8, 8])
        .width(Length::Fill)
        .clip(true)
        .style(style::footer)
        .into()
}

/// The camera part of the settings.
pub struct CameraSettings {
    pub choices: Vec<CameraChoice>,
    pub selected: CameraChoice,
    pub effect: Effect,
    pub mirror: bool,
    pub probing: bool,
    /// What the camera is doing, or why the last search failed.
    pub status: String,
}

/// Everything in the settings popover that the old call bar spread along the
/// top: camera device, effect, mirror, Find cameras, the volume of each
/// person, and the streams to watch.
pub struct Settings<'a> {
    pub camera: Option<CameraSettings>,
    pub voice: &'a VoiceControls,
    /// Streams being watched: (session, caption).
    pub watching: Vec<(std::sync::Arc<playback::Session>, String)>,
    /// Streams on offer: (wire key, owner name).
    pub offers: Vec<(String, String)>,
    /// Watching needs a running call.
    pub running: bool,
}

pub fn settings(set: Settings<'_>) -> Element<'static, Message> {
    let heading = |label: &'static str| text(label).size(11).font(BOLD).style(text::secondary);
    let mut content = column![].spacing(6);
    if let Some(camera) = set.camera {
        content = content.push(heading(tr("CAMERA"))).push(
            pick_list(camera.choices, Some(camera.selected), Message::SelectCamera)
                .text_size(12)
                .width(Length::Fill),
        );
        content = content.push(
            pick_list(
                Effect::ALL,
                Some(camera.effect),
                Message::SelectCameraEffect,
            )
            .text_size(12)
            .width(Length::Fill),
        );
        content = content.push(
            checkbox(camera.mirror)
                .label(tr("Mirror preview"))
                .size(14)
                .text_size(12)
                .on_toggle(Message::SetMirrorPreview),
        );
        content = content.push(
            button(
                text(if camera.probing {
                    tr("Looking for cameras\u{2026}")
                } else {
                    tr("Find cameras")
                })
                .size(12),
            )
            .on_press_maybe((!camera.probing).then_some(Message::ListCameras))
            .style(style::tool_button(false, false)),
        );
        if !camera.status.is_empty() {
            content = content.push(text(camera.status).size(11));
        }
    }
    content = content.push(heading(tr("VOLUME")));
    if set.voice.participants.is_empty() {
        content = content.push(
            text(tr("Nobody else is in the call."))
                .size(11)
                .style(text::secondary),
        );
    }
    for participant in &set.voice.participants {
        let user = participant.user_id;
        let volume = set.voice.volume_percent(user);
        content = content.push(
            column![
                row![
                    container(text(set.voice.display_name(user)).size(12).wrapping(NAME))
                        .width(Length::Fill)
                        .clip(true),
                    button(text("0%").size(11))
                        .on_press(Message::PresetParticipantVolume(user, 0))
                        .style(style::tool_button(false, false)),
                    button(text("100%").size(11))
                        .on_press(Message::PresetParticipantVolume(user, 100))
                        .style(style::tool_button(false, false)),
                    text(format!("{volume}%")).size(12),
                ]
                .spacing(4)
                .align_y(iced::alignment::Vertical::Center),
                slider(0.0..=200.0, volume as f32, move |value| {
                    Message::SetParticipantVolume(user, value.round() as u16)
                })
                .on_release(Message::CommitParticipantVolume(user)),
            ]
            .spacing(2),
        );
    }
    let restricted = match (set.voice.server_muted, set.voice.server_deafened) {
        (true, true) => Some(tr("The server turned off your microphone and sound.")),
        (true, false) => Some(tr("The server turned off your microphone.")),
        (false, true) => Some(tr("The server turned off your sound.")),
        (false, false) => None,
    };
    content = content.push(
        text(restricted.unwrap_or(""))
            .size(11)
            .style(text::secondary),
    );
    if !set.watching.is_empty() || !set.offers.is_empty() {
        content = content.push(heading(tr("STREAMS")));
    }
    for (session, caption) in set.watching {
        content = content.push(
            row![
                container(text(caption).size(12).wrapping(NAME))
                    .width(Length::Fill)
                    .clip(true),
                button(text(tr("Stop watching")).size(11))
                    .on_press(Message::UnwatchStream(session.key.clone()))
                    .style(style::action_button(true)),
            ]
            .spacing(4)
            .align_y(iced::alignment::Vertical::Center),
        );
        content = content.push(stream_audio_controls(&session.key, &session));
    }
    for (key, owner) in set.offers {
        content = content.push(
            button(text(trf("Watch {}", &[&owner])).size(12).wrapping(NAME))
                .on_press_maybe(set.running.then(|| Message::WatchStream(key)))
                .style(style::action_button(false)),
        );
    }
    container(scrollable(
        container(content).padding([8, 10]).width(Length::Fill),
    ))
    .max_height(320.0)
    .width(Length::Fill)
    .clip(true)
    .style(style::footer)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::VoiceParticipant;
    use crate::variable_list::harness::Ui;
    use std::collections::BTreeSet;

    const WIDTH: f32 = crate::navigation::SIDEBAR_WIDTH;

    fn bar_input(sharing: bool, enabled: bool) -> Bar {
        Bar {
            state: VoiceAudioState::Running,
            place: "An extremely long voice channel name / An extremely long server name".into(),
            camera: Some((false, enabled)),
            share: Some((sharing, enabled)),
            soundboard: Some((false, Message::ToggleVoiceDetails)),
            settings_open: false,
            can_leave: true,
            share_text: String::new(),
        }
    }

    /// Every message a click anywhere in the area publishes, as its debug name.
    fn reachable(ui: &mut Ui<'_, Message>, from: i32, to: i32, height: i32) -> BTreeSet<String> {
        let mut hit = BTreeSet::new();
        for y in (0..height).step_by(2) {
            for x in (from..to).step_by(2) {
                for message in ui.click(x as f32, y as f32) {
                    let name = format!("{message:?}");
                    hit.insert(name.split(['(', ' ']).next().unwrap().to_owned());
                }
            }
        }
        hit
    }

    #[test]
    fn the_voice_panel_fits_the_sidebar_with_every_action_reachable() {
        let mut ui = Ui::new(WIDTH, 120.0, bar(bar_input(false, true)));
        let hit = reachable(&mut ui, 0, WIDTH as i32, 120);
        for name in [
            "OpenCall",
            "LeaveVoice",
            "ToggleCamera",
            "OpenSharePicker",
            "ToggleVoiceDetails",
        ] {
            assert!(hit.contains(name), "{name} is not reachable: {hit:?}");
        }
        // Nothing hangs out beyond the sidebar.
        let mut wide = Ui::new(
            WIDTH + 200.0,
            120.0,
            container(bar(bar_input(false, true))).width(WIDTH).into(),
        );
        assert!(reachable(&mut wide, WIDTH as i32, WIDTH as i32 + 200, 120).is_empty());
    }

    #[test]
    fn sharing_turns_the_share_button_into_stop_and_idle_calls_disable_buttons() {
        let mut ui = Ui::new(WIDTH, 120.0, bar(bar_input(true, true)));
        let hit = reachable(&mut ui, 0, WIDTH as i32, 120);
        assert!(hit.contains("StopShare") && !hit.contains("OpenSharePicker"));
        // Connecting: camera and share are off, leaving and the settings stay.
        let mut ui = Ui::new(WIDTH, 120.0, bar(bar_input(false, false)));
        let hit = reachable(&mut ui, 0, WIDTH as i32, 120);
        assert!(!hit.contains("ToggleCamera") && !hit.contains("OpenSharePicker"));
        assert!(hit.contains("LeaveVoice") && hit.contains("ToggleVoiceDetails"));
        let mut input = bar_input(false, true);
        input.can_leave = false;
        let mut ui = Ui::new(WIDTH, 120.0, bar(input));
        assert!(!reachable(&mut ui, 0, WIDTH as i32, 120).contains("LeaveVoice"));
    }

    #[test]
    fn an_activity_shows_a_read_only_note_with_no_join_action() {
        let mut voice = VoiceControls::default();
        assert_eq!(activity_note(&voice), None);
        voice.activities = vec![String::new()];
        assert_eq!(
            activity_note(&voice).as_deref(),
            Some("Running an Activity — join it in the official Discord client")
        );
        voice.activities = vec!["Watch".into(), String::new(), "Draw".into()];
        let note = activity_note(&voice).unwrap();
        assert!(note.ends_with("(Watch, Draw)"), "{note}");
    }

    #[test]
    fn settings_hold_the_controls_of_the_old_top_bar() {
        let voice = VoiceControls {
            participants: vec![VoiceParticipant {
                user_id: Snowflake(2),
                ssrc: None,
                speaking: false,
            }],
            names: vec![(Snowflake(2), "Ann".into())],
            ..VoiceControls::default()
        };
        let first = CameraChoice {
            id: None,
            label: "First camera".into(),
            missing: false,
        };
        let element = settings(Settings {
            camera: Some(CameraSettings {
                choices: vec![first.clone()],
                selected: first,
                effect: Effect::ALL[0],
                mirror: true,
                probing: false,
                status: String::new(),
            }),
            voice: &voice,
            watching: vec![(playback::Session::new("k1".into()), "Ann: live".into())],
            offers: vec![("k2".into(), "Bob".into())],
            running: true,
        });
        let mut ui = Ui::new(WIDTH, 420.0, element);
        let hit = reachable(&mut ui, 0, WIDTH as i32, 420);
        for name in [
            "SetMirrorPreview",
            "ListCameras",
            "PresetParticipantVolume",
            "SetParticipantVolume",
            "UnwatchStream",
            "WatchStream",
        ] {
            assert!(hit.contains(name), "{name} is not reachable: {hit:?}");
        }
    }

    #[test]
    fn stream_audio_controls_fit_the_sidebar_and_route_to_the_selected_watch() {
        let session = playback::Session::new("guild:1:2:3".into());
        let mut ui = Ui::new(
            WIDTH - 20.0,
            90.0,
            stream_audio_controls(&session.key, &session),
        );
        let hit = reachable(&mut ui, 0, WIDTH as i32 - 20, 90);
        for name in ["MuteStream", "SetStreamVolume"] {
            assert!(hit.contains(name), "{name} is not reachable: {hit:?}");
        }
        let messages = ui.click(15.0, 10.0);
        assert!(messages.iter().any(
            |message| matches!(message, Message::MuteStream(key, true) if key == "guild:1:2:3")
        ));
    }
}
