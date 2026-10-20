//! Inline playback of Ogg Opus audio (voice messages), and of MP4 and WebM video (issues
//! #74 and #116): H.264 or AV1 video with Opus audio; AAC is not decoded, so such a video
//! plays without sound and says so.
//!
//! The file comes through the same credential-free, byte-bounded attachment cache as Open
//! (so signed-URL refresh, 401/403/404/429 handling and the 64 MiB ceiling are unchanged),
//! is demuxed, decoded and paced on dedicated threads ([`crate::av_player`]) and its sound is
//! played by an output-only audio engine. Nothing here decodes or touches the disk on the UI
//! thread, and nothing starts without a click: there is no autoplay. One attachment plays at
//! a time; starting another, leaving the channel or scrolling the message out of the built
//! rows drops the session, which stops the threads and releases the output device. Every
//! other audio and video format stays Download/Open-only.
use std::sync::Arc;

use fastcord_audio::{DeviceChoice, PlayerFailure, PlayerSnapshot, PlayerState};
use fastcord_model::{Attachment, Snowflake};
use iced::task::Handle;
use iced::widget::{button, column, container, row, slider, space, text};
use iced::{Element, Length, Task};

use crate::Message;
use crate::attachment_ui::{RestRefresh, safe_attachment_filename};
use crate::attachments::{AttachmentError, AttachmentKey, MAX_FILE_BYTES};
use crate::av_player::{MediaPlayer, StartError};
use crate::gateway::GatewayPanel;
use crate::login::Session;
use crate::timeline::Event;

/// A started player on its way to the reducer; sharing, not copying, the one player.
#[derive(Clone)]
pub struct PlayerHandle(Arc<MediaPlayer>);

impl std::fmt::Debug for PlayerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PlayerHandle")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Fetching the file and opening the output device.
    Loading,
    Active,
    /// Why it cannot play (a fixed, secret-free message).
    Failed(&'static str),
}

/// What the user does to the playing attachment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    TogglePause,
    /// The seek slider moved (milliseconds); applied on release.
    SeekPreview(u32),
    SeekCommit,
    /// Volume 0-100.
    Volume(u8),
    ToggleMute,
    Close,
}

/// The one attachment being played, if any.
pub struct MediaSession {
    pub key: AttachmentKey,
    pub phase: Phase,
    pub snapshot: PlayerSnapshot,
    pub volume: u8,
    pub muted: bool,
    seeking: Option<u32>,
    player: Option<Arc<MediaPlayer>>,
    _worker: Handle,
}

impl std::fmt::Debug for MediaSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaSession")
            .field("key", &self.key)
            .field("phase", &self.phase)
            .finish_non_exhaustive()
    }
}

impl MediaSession {
    fn new(key: AttachmentKey, worker: Handle) -> Self {
        Self {
            key,
            phase: Phase::Loading,
            snapshot: PlayerSnapshot {
                state: PlayerState::Playing,
                position_ms: 0,
                duration_ms: None,
            },
            volume: 100,
            muted: false,
            seeking: None,
            player: None,
            _worker: worker,
        }
    }

    #[cfg(test)]
    pub fn for_test(key: AttachmentKey) -> Self {
        let (_, worker) = Task::<()>::none().abortable();
        Self::new(key, worker.abort_on_drop())
    }

    /// The latest-frame slot of a playing video, for the frame subscription.
    pub fn video_session(&self) -> Option<Arc<crate::playback::Session>> {
        self.player
            .as_ref()
            .and_then(|player| player.video())
            .map(|(session, _)| session)
    }

    /// Whether the position needs periodic refreshing.
    pub fn playing(&self) -> bool {
        self.phase == Phase::Active && self.snapshot.state == PlayerState::Playing
    }
}

/// Offered for Ogg Opus, MP4 and WebM by name or type; the file itself (its container, codecs
/// and tracks) decides whether it really plays.
pub fn is_playable(attachment: &Attachment) -> bool {
    let named = std::path::Path::new(&attachment.filename)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            ["ogg", "opus", "oga", "mp4", "m4v", "mov", "webm", "mkv"]
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        });
    let typed = attachment
        .content_type
        .as_deref()
        .is_some_and(|content_type| {
            let mime = content_type.split(';').next().unwrap_or_default().trim();
            [
                "audio/ogg",
                "audio/opus",
                "video/mp4",
                "video/webm",
                "video/quicktime",
                "video/x-matroska",
            ]
            .iter()
            .any(|known| mime.eq_ignore_ascii_case(known))
        });
    (named || typed) && attachment.size <= MAX_FILE_BYTES
}

fn failure_reason(failure: PlayerFailure) -> &'static str {
    match failure {
        PlayerFailure::Device => "The audio output stopped. Check the device and try again.",
        PlayerFailure::Damaged => "This audio file is damaged. Use Download.",
        PlayerFailure::Unsupported => {
            "This audio format cannot be played here. Use Download or Open."
        }
    }
}

fn start_reason(error: StartError) -> &'static str {
    match error {
        StartError::Damaged => "This file is damaged. Use Download.",
        StartError::UnsupportedAudio => {
            "This audio format cannot be played here. Use Download or Open."
        }
        StartError::UnsupportedVideo => {
            "This video format cannot be played here (only H.264 or AV1 in MP4 or WebM). Use Download or Open."
        }
        StartError::UnsupportedLayout => {
            "This video is stored in a layout that cannot be played here. Use Download or Open."
        }
        StartError::TooLarge => {
            "This media file is too large or complex to play here. Use Download."
        }
        StartError::Device => {
            "The audio output could not be opened. Check the device in audio settings."
        }
        StartError::Decoder => {
            "No video decoder is available for this file on this device. Use Download or Open."
        }
    }
}

fn fetch_reason(error: &AttachmentError) -> &'static str {
    match error {
        AttachmentError::TooLarge => {
            "This file is larger than the 64 MiB playback limit. Use Download."
        }
        AttachmentError::WorkerStopped => "Playback was cancelled.",
        AttachmentError::CacheUnavailable => "The local attachment cache is unavailable.",
        _ => "The file could not be fetched. Try again.",
    }
}

/// Starts (or restarts) playback of one attachment, replacing any other session.
pub fn play(
    gateway: &mut GatewayPanel,
    session: &Session,
    output: DeviceChoice,
    channel: Snowflake,
    message: Snowflake,
    attachment: Snowflake,
) -> Task<Message> {
    if gateway.timeline.channel_id != Some(channel) {
        return Task::none();
    }
    let key = AttachmentKey {
        account: session.user.id,
        channel,
        message,
        attachment,
    };
    if gateway.media.as_ref().is_some_and(|media| {
        media.key == key && media.phase != Phase::Loading && media.player.is_some()
    }) {
        return Task::none();
    }
    let Some(item) = gateway
        .timeline
        .rows
        .iter()
        .find(|row| row.message.id == message)
        .and_then(|row| row.message.attachment(attachment))
        .cloned()
        .filter(is_playable)
    else {
        return Task::none();
    };
    let Some(cache) = gateway.attachments.clone() else {
        return Task::none();
    };
    let refresh = RestRefresh(session.client.clone());
    let generation = gateway.id;
    let filename = safe_attachment_filename(&item.filename, item.content_type.as_deref());
    let (task, worker) = Task::perform(
        async move {
            let path = cache
                .fetch_file(&refresh, key, &filename, &item.url, MAX_FILE_BYTES)
                .await
                .map_err(|error| fetch_reason(&error))?;
            // Opening the file and the device can block; neither belongs on the UI thread.
            tokio::task::spawn_blocking(move || MediaPlayer::start(&path, output))
                .await
                .map_err(|_| "Playback was cancelled.")?
                .map(|player| PlayerHandle(Arc::new(player)))
                .map_err(start_reason)
        },
        move |result| Message::MediaReady(generation, key, result),
    )
    .abortable();
    // Replacing the session drops the previous player and aborts a pending load.
    gateway.media = Some(MediaSession::new(key, worker.abort_on_drop()));
    task
}

/// A started player (or the reason it could not start) arrived.
pub fn ready(
    gateway: &mut GatewayPanel,
    key: AttachmentKey,
    result: Result<PlayerHandle, &'static str>,
) {
    let Some(media) = gateway
        .media
        .as_mut()
        .filter(|media| media.key == key && media.phase == Phase::Loading)
    else {
        // Superseded or closed: dropping the handle stops the player.
        return;
    };
    match result {
        Ok(PlayerHandle(player)) => {
            media.snapshot = player.snapshot();
            media.player = Some(player);
            media.phase = Phase::Active;
        }
        Err(reason) => media.phase = Phase::Failed(reason),
    }
}

/// Refreshes the position and notices an ended or failed player.
pub fn tick(gateway: &mut GatewayPanel) {
    let Some(media) = gateway.media.as_mut() else {
        return;
    };
    let Some(player) = media.player.as_ref() else {
        return;
    };
    media.snapshot = player.snapshot();
    if let PlayerState::Failed(failure) = media.snapshot.state {
        media.phase = Phase::Failed(failure_reason(failure));
        media.player = None;
    }
}

pub fn control(gateway: &mut GatewayPanel, control: Control) {
    if control == Control::Close {
        gateway.media = None;
        return;
    }
    let Some(media) = gateway.media.as_mut() else {
        return;
    };
    let Some(player) = media
        .player
        .clone()
        .filter(|_| media.phase == Phase::Active)
    else {
        return;
    };
    match control {
        Control::TogglePause => {
            // Optimistic: the player thread confirms on the next tick.
            if media.snapshot.state == PlayerState::Playing {
                player.pause();
                media.snapshot.state = PlayerState::Paused;
            } else {
                player.play();
                media.snapshot.state = PlayerState::Playing;
            }
        }
        Control::SeekPreview(ms) => media.seeking = Some(ms),
        Control::SeekCommit => {
            if let Some(ms) = media.seeking.take() {
                player.seek(u64::from(ms));
                media.snapshot.position_ms = u64::from(ms);
                if media.snapshot.state == PlayerState::Ended {
                    media.snapshot.state = PlayerState::Paused;
                }
            }
        }
        Control::Volume(volume) => {
            media.volume = volume.min(100);
            media.muted = false;
            player.set_muted(false);
            player.set_volume(f32::from(media.volume) / 100.0);
        }
        Control::ToggleMute => {
            media.muted = !media.muted;
            player.set_muted(media.muted);
        }
        Control::Close => {}
    }
}

/// The inline picture size: at most 360 x 240 logical pixels, at most twice the video's own.
fn video_box(width: u32, height: u32) -> (f32, f32) {
    let (width, height) = (width.max(1) as f32, height.max(1) as f32);
    let scale = (360.0 / width).min(240.0 / height).min(2.0);
    (
        (width * scale).round().max(1.0),
        (height * scale).round().max(1.0),
    )
}

fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Most bars a voice message's waveform is drawn with.
const WAVEFORM_BARS: usize = 64;

/// The loudness bars (0-255) of an attachment's `waveform`, the loudest sample of each group
/// when there are more than [`WAVEFORM_BARS`]; empty without valid metadata.
pub fn waveform_bars(waveform: Option<&str>) -> Vec<u8> {
    group_bars(
        waveform
            .map(fastcord_discord::decode_waveform)
            .unwrap_or_default(),
    )
}

/// [`waveform_bars`] for samples already decoded.
pub fn group_bars(samples: Vec<u8>) -> Vec<u8> {
    if samples.len() <= WAVEFORM_BARS {
        return samples;
    }
    (0..WAVEFORM_BARS)
        .map(|bar| {
            let from = bar * samples.len() / WAVEFORM_BARS;
            let to = ((bar + 1) * samples.len() / WAVEFORM_BARS).max(from + 1);
            samples[from..to.min(samples.len())]
                .iter()
                .copied()
                .max()
                .unwrap_or(0)
        })
        .collect()
}

/// A voice message's length, from the sender's metadata.
fn metadata_duration_ms(attachment: &Attachment) -> Option<u64> {
    attachment
        .duration_secs
        .map(|secs| (secs.max(0.0) * 1000.0) as u64)
}

/// Loudness bars; the share `progress` of them (0-1) is drawn as already played.
pub fn waveform_view<M: 'static>(bars: &[u8], progress: f32) -> Element<'static, M> {
    let played = (progress.clamp(0.0, 1.0) * bars.len() as f32).round() as usize;
    row(bars.iter().enumerate().map(|(index, &level)| {
        container(space())
            .width(3)
            .height(4.0 + f32::from(level) / 255.0 * 20.0)
            .style(move |theme: &iced::Theme| {
                let palette = theme.extended_palette();
                container::Style {
                    background: Some(
                        if index < played {
                            palette.primary.strong.color
                        } else {
                            palette.background.strong.color
                        }
                        .into(),
                    ),
                    border: iced::Border {
                        radius: 1.5.into(),
                        ..iced::Border::default()
                    },
                    ..container::Style::default()
                }
            })
            .into()
    }))
    .spacing(1)
    .align_y(iced::Alignment::Center)
    .height(24)
    .into()
}

/// The Play button, loading line, or player controls for one attachment. `voice` is whether
/// Discord flagged the message as a voice message; a waveform or length in the attachment's
/// own metadata makes it one too.
pub fn view(
    media: Option<&MediaSession>,
    key: AttachmentKey,
    attachment: &Attachment,
    voice: bool,
) -> Element<'static, Event> {
    let bars = waveform_bars(attachment.waveform.as_deref());
    let voice = voice || !bars.is_empty() || attachment.duration_secs.is_some();
    let length = metadata_duration_ms(attachment);
    let ids = (key.channel, key.message, key.attachment);
    let play = |label: &'static str| {
        button(text(label).size(13))
            .padding([2, 8])
            .on_press(Event::PlayAttachment {
                channel_id: ids.0,
                message_id: ids.1,
                attachment_id: ids.2,
            })
    };
    let small = |label: &'static str, control: Control| {
        button(text(label).size(13))
            .padding([2, 8])
            .on_press(Event::Media(control))
    };
    let Some(media) = media.filter(|media| media.key == key) else {
        if !voice {
            return play("Play").into();
        }
        let mut line = row![play("Play")]
            .spacing(8)
            .align_y(iced::Alignment::Center);
        if !bars.is_empty() {
            line = line.push(waveform_view(&bars, 0.0));
        }
        return line
            .push(
                text(length.map_or_else(|| "Voice message".to_owned(), clock))
                    .size(12)
                    .style(text::secondary),
            )
            .into();
    };
    match media.phase {
        Phase::Loading => row![
            text("Loading…").size(12).style(text::secondary),
            small("Cancel", Control::Close)
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into(),
        Phase::Failed(reason) => row![
            text(reason).size(12).style(text::secondary),
            play("Retry"),
            small("Close", Control::Close)
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into(),
        Phase::Active => {
            let snapshot = media.snapshot;
            let position = media.seeking.map_or(snapshot.position_ms, u64::from);
            let toggle = match snapshot.state {
                PlayerState::Playing => small("Pause", Control::TogglePause),
                PlayerState::Ended => small("Replay", Control::TogglePause),
                _ => small("Play", Control::TogglePause),
            };
            let mut controls = row![toggle].spacing(8).align_y(iced::Alignment::Center);
            if let Some(duration) = snapshot.duration_ms {
                let end = duration.min(u64::from(u32::MAX)) as u32;
                let at = (position.min(u64::from(end))) as u32;
                controls = controls
                    .push(
                        slider(0..=end, at, |ms| Event::Media(Control::SeekPreview(ms)))
                            .on_release(Event::Media(Control::SeekCommit))
                            .width(Length::Fixed(160.0)),
                    )
                    .push(
                        text(format!("{} / {}", clock(position), clock(duration)))
                            .size(12)
                            .style(text::secondary),
                    );
            } else {
                controls = controls.push(text(clock(position)).size(12).style(text::secondary));
            }
            let controls = controls
                .push(small(
                    if media.muted { "Unmute" } else { "Mute" },
                    Control::ToggleMute,
                ))
                .push(
                    slider(
                        0..=100u8,
                        if media.muted { 0 } else { media.volume },
                        |volume| Event::Media(Control::Volume(volume)),
                    )
                    .width(Length::Fixed(70.0)),
                )
                .push(small("Close", Control::Close));
            let mut stack = column![].spacing(4);
            if voice && !bars.is_empty() {
                let total = snapshot.duration_ms.or(length).filter(|total| *total > 0);
                let progress = total.map_or(0.0, |total| position as f32 / total as f32);
                stack = stack.push(waveform_view(&bars, progress));
            }
            if let Some((session, (width, height))) =
                media.player.as_ref().and_then(|player| player.video())
            {
                let (width, height) = video_box(width, height);
                stack = stack.push(
                    container(crate::playback::view::<Event>(session))
                        .width(Length::Fixed(width))
                        .height(Length::Fixed(height)),
                );
            }
            stack = stack.push(controls);
            if let Some(player) = &media.player {
                if player.video_failed() {
                    stack = stack.push(
                        text("The video could not be decoded; any sound continues.")
                            .size(12)
                            .style(text::secondary),
                    );
                }
                if let Some(notice) = player.notice() {
                    stack = stack.push(text(notice).size(12).style(text::secondary));
                }
            }
            stack.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::NavigationBridge;

    fn attachment(filename: &str, content_type: Option<&str>, size: u64) -> Attachment {
        Attachment {
            id: Snowflake(1),
            filename: filename.to_owned(),
            size,
            url: "https://cdn.example/a".to_owned(),
            proxy_url: None,
            content_type: content_type.map(str::to_owned),
            width: None,
            height: None,
            duration_secs: None,
            waveform: None,
        }
    }

    fn key() -> AttachmentKey {
        AttachmentKey {
            account: Snowflake(1),
            channel: Snowflake(2),
            message: Snowflake(3),
            attachment: Snowflake(4),
        }
    }

    fn panel() -> GatewayPanel {
        let (_, worker) = Task::<()>::none().abortable();
        GatewayPanel::new(1, worker, NavigationBridge::new())
    }

    #[test]
    fn only_ogg_mp4_and_webm_candidates_within_the_cache_limit_are_offered() {
        for (name, kind) in [
            ("voice-message.ogg", Some("audio/ogg")),
            ("song.OPUS", None),
            ("clip.oga", Some("application/octet-stream")),
            ("noext", Some("audio/ogg; codecs=opus")),
            ("clip.MP4", Some("video/mp4")),
            ("clip.webm", Some("video/webm")),
            ("screen.mov", None),
            ("noext", Some("video/webm; codecs=vp9")),
        ] {
            assert!(is_playable(&attachment(name, kind, 10)), "{name}");
        }
        for (name, kind) in [
            ("song.mp3", Some("audio/mpeg")),
            ("movie.avi", Some("video/x-msvideo")),
            ("clip.exe", Some("application/x-msdownload")),
            ("a.wav", Some("audio/wav")),
            ("a.m4a", Some("audio/mp4")),
            ("a.flac", None),
            ("a.ogg.exe", None),
        ] {
            assert!(!is_playable(&attachment(name, kind, 10)), "{name}");
        }
        assert!(!is_playable(&attachment(
            "big.ogg",
            None,
            MAX_FILE_BYTES + 1
        )));
    }

    #[test]
    fn a_stale_ready_result_is_ignored_and_a_failure_is_shown() {
        let mut gateway = panel();
        ready(&mut gateway, key(), Err("no"));
        assert!(gateway.media.is_none());
        gateway.media = Some(MediaSession::for_test(key()));
        let mut other = key();
        other.attachment = Snowflake(5);
        ready(&mut gateway, other, Err("stale"));
        assert_eq!(gateway.media.as_ref().unwrap().phase, Phase::Loading);
        ready(&mut gateway, key(), Err("no"));
        assert_eq!(gateway.media.as_ref().unwrap().phase, Phase::Failed("no"));
        // Controls on a session without a player do nothing, Close always works.
        control(&mut gateway, Control::TogglePause);
        control(&mut gateway, Control::Close);
        assert!(gateway.media.is_none());
    }

    #[test]
    fn the_inline_picture_keeps_its_aspect_and_stays_small() {
        assert_eq!(video_box(1920, 1080), (360.0, 203.0));
        assert_eq!(video_box(1080, 1920), (135.0, 240.0));
        assert_eq!(video_box(128, 96), (256.0, 192.0));
        assert_eq!(video_box(0, 0), (2.0, 2.0));
    }

    #[test]
    fn every_start_failure_has_its_own_message() {
        use std::collections::HashSet;
        let all = [
            StartError::Damaged,
            StartError::UnsupportedAudio,
            StartError::UnsupportedVideo,
            StartError::UnsupportedLayout,
            StartError::TooLarge,
            StartError::Device,
            StartError::Decoder,
        ];
        let reasons: HashSet<_> = all.into_iter().map(start_reason).collect();
        assert_eq!(reasons.len(), all.len());
    }

    #[test]
    fn the_clock_shows_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(59_999), "0:59");
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(3_600_000), "60:00");
    }

    #[test]
    fn voice_waveforms_become_at_most_sixty_four_bars_and_the_play_row_appears() {
        // Standard base64 with padding, written out so the app needs no encoder.
        let encode = |bytes: &[u8]| {
            const SET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut out = String::new();
            for chunk in bytes.chunks(3) {
                let n = chunk
                    .iter()
                    .enumerate()
                    .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
                for i in 0..4 {
                    out.push(if i <= chunk.len() {
                        char::from(SET[(n >> (18 - 6 * i) & 63) as usize])
                    } else {
                        '='
                    });
                }
            }
            out
        };
        assert!(waveform_bars(None).is_empty());
        assert!(waveform_bars(Some("###")).is_empty());
        assert_eq!(waveform_bars(Some(&encode(&[1, 2, 3]))), vec![1, 2, 3]);
        let full: Vec<u8> = (0..=255).collect();
        let bars = waveform_bars(Some(&encode(&full)));
        assert_eq!(bars.len(), 64);
        assert_eq!(bars[63], 255);
        assert!(bars.windows(2).all(|pair| pair[0] <= pair[1]));
        let mut voice = attachment("voice-message.ogg", Some("audio/ogg"), 10);
        voice.duration_secs = Some(3.5);
        voice.waveform = Some(encode(&full));
        // Idle, loading and active voice rows all build (the metadata is only presentation).
        let _ = view(None, key(), &voice, true);
        let mut media = MediaSession::for_test(key());
        let _ = view(Some(&media), key(), &voice, true);
        media.phase = Phase::Failed("no");
        let _ = view(Some(&media), key(), &voice, false);
        assert_eq!(metadata_duration_ms(&voice), Some(3500));
        assert_eq!(metadata_duration_ms(&attachment("a.ogg", None, 1)), None);
    }
}
