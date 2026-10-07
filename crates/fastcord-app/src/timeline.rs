//! The message timeline of the selected channel: the viewport-only read model
//! the account worker publishes, the events the view emits, and the view.
//!
//! A [`Snapshot`] holds only the rows [`Window`] asks to be built, each as a
//! shared `Arc<Message>`, plus the numeric spacers standing in for everything
//! else; the worker owns the history and the [`VariableList`] behind it. The
//! view never fetches, sorts, or caches anything: it lays the rows out, and the
//! widget in [`variable_list`] measures what that layout really produced and
//! reports it as [`Event`]s that carry the channel they were produced for, so a
//! late event for a channel that is no longer open is easy to reject.
//!
//! Attachments are listed by filename and metadata only. Image fetching and
//! thumbnails belong to the attachment-viewing milestone; rows will simply
//! grow when they arrive, and the measured-height path keeps the reader's
//! position when they do.
//!
//! [`VariableList`]: crate::variable_list::VariableList
use std::sync::Arc;

use fastcord_model::{Attachment, Message, Snowflake};
use iced::alignment::{Horizontal, Vertical};
use iced::font::{Font, Weight};
use iced::widget::{button, column, container, row, stack, text};
use iced::{Element, Length, Padding};

use crate::variable_list::{self, Item, Measurement, Report, ScrollRequest, Viewport, Window};

/// Closer to the first retained message than this, the view offers to load older ones.
const TOP_THRESHOLD: f32 = 80.0;
/// Timestamps that are not ISO 8601 are shown truncated to this many characters.
const MAX_TIMESTAMP_CHARS: usize = 32;
const BOLD: Font = Font {
    weight: Weight::Bold,
    ..Font::DEFAULT
};

/// One built row: the message (shared with the worker's store, never copied) and
/// the revision of its body, which tells the height cache when to distrust itself.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub message: Arc<Message>,
    pub revision: u64,
}

/// Everything the timeline view needs, and nothing else. `rows` are exactly
/// the messages of `window.range` (or a prefix-trimmed version of it whose
/// spacers were recomputed with `VariableList::window_for_range`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// The channel the rows belong to; events carry it back.
    pub channel_id: Option<Snowflake>,
    pub rows: Vec<Row>,
    pub window: Window,
    /// The list's unacknowledged scroll request; copy it into every snapshot.
    pub scroll: Option<ScrollRequest>,
    /// A page is being fetched.
    pub loading: bool,
    /// Older messages exist beyond the retained rows.
    pub has_older: bool,
    /// Newer messages exist beyond the retained rows (the newest end was
    /// evicted, or arrived while the reader was away from the bottom).
    pub has_newer: bool,
    /// A fixed, secret-free description of why the last page failed.
    pub error: Option<&'static str>,
}

/// What the timeline asks of the worker. Each event names its channel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Event {
    /// The widget's latest view of the viewport (coalesce to the newest).
    Viewport {
        channel_id: Snowflake,
        viewport: Viewport,
    },
    /// The height a row actually laid out to (keep the newest per row and width bucket).
    Measured {
        channel_id: Snowflake,
        measurement: Measurement,
    },
    /// Jump to the newest message, reloading it first when it is not retained.
    JumpLatest {
        channel_id: Snowflake,
    },
    LoadOlder {
        channel_id: Snowflake,
    },
    Retry {
        channel_id: Snowflake,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Notice {
    Loading,
    Failed(&'static str),
    LoadOlder,
}

/// Which extras the view shows, decided from the snapshot alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Controls {
    /// Centered text instead of rows.
    empty: Option<&'static str>,
    /// A pill at the top.
    notice: Option<Notice>,
    /// The jump-to-latest button.
    jump: bool,
}

impl Controls {
    fn of(snapshot: &Snapshot) -> Self {
        let has_rows = !snapshot.rows.is_empty();
        let notice = match snapshot.error {
            Some(message) => Some(Notice::Failed(message)),
            None if has_rows && snapshot.loading => Some(Notice::Loading),
            None if has_rows && snapshot.has_older && snapshot.window.offset <= TOP_THRESHOLD => {
                Some(Notice::LoadOlder)
            }
            None => None,
        };
        Self {
            empty: (!has_rows && snapshot.error.is_none()).then_some(if snapshot.loading {
                "Loading messages..."
            } else {
                "There are no messages here yet."
            }),
            notice,
            jump: has_rows && (!snapshot.window.at_bottom || snapshot.has_newer),
        }
    }
}

pub fn view(snapshot: &Snapshot) -> Element<'_, Event> {
    let Some(channel_id) = snapshot.channel_id else {
        return centered("Select a channel to read its messages.");
    };
    let rows = snapshot.rows.iter().map(|row| {
        (
            Item {
                id: row.message.id,
                revision: row.revision,
            },
            message_view(&row.message),
        )
    });
    let list = variable_list::view(
        channel_id.0,
        &snapshot.window,
        snapshot.scroll,
        rows,
        move |report| match report {
            Report::Viewport(viewport) => Event::Viewport {
                channel_id,
                viewport,
            },
            Report::Measured(measurement) => Event::Measured {
                channel_id,
                measurement,
            },
        },
    );
    let controls = Controls::of(snapshot);
    let mut layers = vec![list];
    if let Some(message) = controls.empty {
        layers.push(centered(message));
    }
    if let Some(notice) = controls.notice {
        layers.push(notice_layer(notice, channel_id));
    }
    if controls.jump {
        layers.push(jump_layer(channel_id));
    }
    stack(layers)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn centered(message: &'static str) -> Element<'static, Event> {
    container(text(message).size(14).style(text::secondary))
        .center(Length::Fill)
        .into()
}

fn pill_button(label: &'static str, event: Event) -> Element<'static, Event> {
    button(text(label).size(13))
        .on_press(event)
        .padding(Padding {
            top: 4.0,
            right: 12.0,
            bottom: 4.0,
            left: 12.0,
        })
        .style(button::secondary)
        .into()
}

fn notice_layer(notice: Notice, channel_id: Snowflake) -> Element<'static, Event> {
    let content: Element<'static, Event> = match notice {
        Notice::Loading => text("Loading messages...").size(13).into(),
        Notice::Failed(message) => row![
            text(message).size(13).style(text::danger),
            pill_button("Retry", Event::Retry { channel_id }),
        ]
        .spacing(12)
        .align_y(Vertical::Center)
        .into(),
        Notice::LoadOlder => pill_button("Load older messages", Event::LoadOlder { channel_id }),
    };
    container(container(content).padding(8).style(container::rounded_box))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(Horizontal::Center)
        .align_y(Vertical::Top)
        .padding(8)
        .into()
}

fn jump_layer(channel_id: Snowflake) -> Element<'static, Event> {
    container(
        button(text("Jump to latest").size(13))
            .on_press(Event::JumpLatest { channel_id })
            .padding(Padding {
                top: 6.0,
                right: 14.0,
                bottom: 6.0,
                left: 14.0,
            })
            .style(button::primary),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(Horizontal::Right)
    .align_y(Vertical::Bottom)
    .padding(16)
    .into()
}

/// Author, time, wrapped content, and attachment lines. Plain text only: no
/// embeds, markdown, or emoji assets yet.
fn message_view(message: &Message) -> Element<'_, Event> {
    let mut header = row![
        text(message.author.display_name()).font(BOLD).size(15),
        text(format_timestamp(&message.timestamp))
            .size(12)
            .style(text::secondary),
    ]
    .spacing(8)
    .align_y(Vertical::Center);
    if message.edited_timestamp.is_some() {
        header = header.push(text("(edited)").size(12).style(text::secondary));
    }
    let mut body = column![header].spacing(2);
    if !message.content.is_empty() {
        body = body.push(
            text(message.content.as_str())
                .size(14)
                .width(Length::Fill)
                .wrapping(text::Wrapping::WordOrGlyph),
        );
    } else if let Some(note) = placeholder(message) {
        body = body.push(text(note).size(13).style(text::secondary));
    }
    for attachment in &message.attachments {
        body = body.push(
            text(describe_attachment(attachment))
                .size(13)
                .width(Length::Fill)
                .wrapping(text::Wrapping::WordOrGlyph)
                .style(text::secondary),
        );
    }
    container(body)
        .width(Length::Fill)
        .padding(Padding {
            top: 6.0,
            right: 16.0,
            bottom: 6.0,
            left: 12.0,
        })
        .into()
}

/// What to show for a message without text or attachments.
fn placeholder(message: &Message) -> Option<String> {
    if !message.attachments.is_empty() {
        return None;
    }
    Some(match message.kind {
        0 | 19 | 20 | 23 => "No text content.".to_owned(),
        kind => format!("System message (type {kind})."),
    })
}

fn describe_attachment(attachment: &Attachment) -> String {
    let mut details = vec![format_size(attachment.size)];
    if let Some(kind) = attachment.content_type.as_deref() {
        details.push(kind.to_owned());
    }
    if let (Some(width), Some(height)) = (attachment.width, attachment.height) {
        details.push(format!("{width}x{height}"));
    }
    format!(
        "Attachment: {} ({})",
        attachment.filename,
        details.join(", ")
    )
}

fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// `2026-10-07T11:32:01.508000+00:00` as `2026-10-07 11:32 UTC`. Offsets other
/// than UTC are shown as written, since there is no time-zone database here;
/// anything that is not ISO 8601 is shown truncated rather than guessed at.
fn format_timestamp(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let iso = bytes.len() >= 16
        && digits(0..4)
        && bytes[4] == b'-'
        && digits(5..7)
        && bytes[7] == b'-'
        && digits(8..10)
        && matches!(bytes[10], b'T' | b' ')
        && digits(11..13)
        && bytes[13] == b':'
        && digits(14..16);
    if !iso {
        return raw.chars().take(MAX_TIMESTAMP_CHARS).collect();
    }
    // The first 16 bytes are ASCII, so these slices are on character boundaries.
    let tail = &raw[16..];
    let zone = tail
        .find(['+', '-', 'Z'])
        .map_or("", |at| &tail[at..])
        .chars()
        .take(6)
        .collect::<String>();
    let zone = match zone.as_str() {
        "" | "Z" | "+00:00" | "-00:00" => "UTC",
        other => other,
    };
    format!("{} {} {zone}", &raw[..10], &raw[11..16])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variable_list::harness::Ui;
    use iced_renderer::core::widget::Tree;

    fn message(n: u64, content: &str) -> Arc<Message> {
        Arc::new(
            serde_json::from_value(serde_json::json!({
                "id": n.to_string(),
                "channel_id": "500",
                "author": {"id": "2", "username": "alt", "global_name": "Alt Account"},
                "content": content,
                "timestamp": "2026-10-07T12:00:00.000000+00:00",
                "attachments": [{
                    "id": "9",
                    "filename": "cat.png",
                    "size": 12345,
                    "url": "https://cdn.example/a/cat.png?ex=SIGNED",
                    "content_type": "image/png",
                    "width": 640,
                    "height": 480
                }],
            }))
            .expect("valid message"),
        )
    }

    fn snapshot(rows: u64, history: usize) -> Snapshot {
        let rows: Vec<Row> = (1..=rows)
            .map(|n| Row {
                message: message(n, "hello world"),
                revision: 1,
            })
            .collect();
        let built = rows.len();
        Snapshot {
            channel_id: Some(Snowflake(500)),
            window: Window {
                range: 0..built,
                top: 0.0,
                bottom: (history - built) as f32 * 56.0,
                at_bottom: true,
                ..Window::default()
            },
            rows,
            ..Snapshot::default()
        }
    }

    fn nodes(tree: &Tree) -> usize {
        1 + tree.children.iter().map(nodes).sum::<usize>()
    }

    fn built_nodes(snapshot: &Snapshot) -> usize {
        let element = view(snapshot);
        nodes(&Tree::new(&element))
    }

    #[test]
    fn built_widgets_follow_the_rows_not_the_history() {
        // The same ten rows out of a 100- or a 500-message history cost the same.
        assert_eq!(
            built_nodes(&snapshot(10, 100)),
            built_nodes(&snapshot(10, 500))
        );
        // Every extra row costs the same number of widgets.
        let ten = built_nodes(&snapshot(10, 500));
        let twenty = built_nodes(&snapshot(20, 500));
        let thirty = built_nodes(&snapshot(30, 500));
        assert!(twenty > ten);
        assert_eq!(thirty - twenty, twenty - ten);
        // No channel, or none loaded yet, is a placeholder rather than an error.
        assert!(built_nodes(&Snapshot::default()) < ten);
    }

    #[test]
    fn controls_follow_the_snapshot() {
        let mut state = snapshot(5, 5);
        assert_eq!(
            Controls::of(&state),
            Controls {
                empty: None,
                notice: None,
                jump: false
            }
        );
        // Away from the bottom, or with newer messages out of reach, jumping is offered.
        state.window.at_bottom = false;
        assert!(Controls::of(&state).jump);
        state.window.at_bottom = true;
        state.has_newer = true;
        assert!(Controls::of(&state).jump);
        state.has_newer = false;
        // Older messages are offered only near the top and when idle.
        state.has_older = true;
        state.window.offset = 10.0;
        assert_eq!(Controls::of(&state).notice, Some(Notice::LoadOlder));
        state.window.offset = 5_000.0;
        assert_eq!(Controls::of(&state).notice, None);
        state.window.offset = 10.0;
        state.loading = true;
        assert_eq!(Controls::of(&state).notice, Some(Notice::Loading));
        state.error = Some("history failed");
        assert_eq!(
            Controls::of(&state).notice,
            Some(Notice::Failed("history failed"))
        );
        // Nothing loaded yet.
        let mut empty = Snapshot {
            channel_id: Some(Snowflake(500)),
            loading: true,
            ..Snapshot::default()
        };
        assert_eq!(
            Controls::of(&empty),
            Controls {
                empty: Some("Loading messages..."),
                notice: None,
                jump: false
            }
        );
        empty.loading = false;
        assert_eq!(
            Controls::of(&empty).empty,
            Some("There are no messages here yet.")
        );
        empty.error = Some("history failed");
        assert_eq!(
            Controls::of(&empty),
            Controls {
                empty: None,
                notice: Some(Notice::Failed("history failed")),
                jump: false
            }
        );
    }

    #[test]
    fn debug_output_never_contains_bodies_or_signed_urls() {
        let state = Snapshot {
            rows: vec![Row {
                message: message(7, "private conversation"),
                revision: 3,
            }],
            ..snapshot(0, 1)
        };
        let printed = format!("{state:?} {:?}", state.rows[0]);
        assert!(!printed.contains("private conversation"), "{printed}");
        assert!(
            !printed.contains("SIGNED") && !printed.contains("cdn.example"),
            "{printed}"
        );
        assert!(printed.contains('7'), "ids still identify rows: {printed}");
    }

    #[test]
    fn events_name_their_channel() {
        let event = Event::Measured {
            channel_id: Snowflake(500),
            measurement: Measurement {
                id: Snowflake(1),
                revision: 1,
                width_bucket: 20,
                height: 80.0,
            },
        };
        assert_eq!(event, event);
        assert_ne!(
            Event::JumpLatest {
                channel_id: Snowflake(500)
            },
            Event::JumpLatest {
                channel_id: Snowflake(501)
            }
        );
    }

    #[test]
    fn timestamps_read_as_utc_or_as_written() {
        assert_eq!(
            format_timestamp("2026-10-07T11:32:01.508000+00:00"),
            "2026-10-07 11:32 UTC"
        );
        assert_eq!(
            format_timestamp("2026-10-07T11:32:01Z"),
            "2026-10-07 11:32 UTC"
        );
        assert_eq!(
            format_timestamp("2026-10-07T11:32:01.5+02:00"),
            "2026-10-07 11:32 +02:00"
        );
        assert_eq!(format_timestamp("2026-10-07T11:32"), "2026-10-07 11:32 UTC");
        for odd in ["", "yesterday", "2026-13", "２０２６-10-07T11:32:01+00:00"] {
            assert_eq!(format_timestamp(odd), odd);
        }
        let long = "x".repeat(200);
        assert_eq!(format_timestamp(&long).chars().count(), MAX_TIMESTAMP_CHARS);
    }

    #[test]
    fn sizes_and_attachments_are_described() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(5 * 1024 * 1024), "5.0 MB");
        assert!(format_size(u64::MAX).ends_with(" TB"));
        let message = message(1, "");
        assert_eq!(
            describe_attachment(&message.attachments[0]),
            "Attachment: cat.png (12.1 KB, image/png, 640x480)"
        );
        // Text-less messages say so unless an attachment explains the row.
        assert_eq!(placeholder(&message), None);
        let mut bare = (*message).clone();
        bare.attachments.clear();
        assert_eq!(placeholder(&bare).as_deref(), Some("No text content."));
        bare.kind = 7;
        assert_eq!(
            placeholder(&bare).as_deref(),
            Some("System message (type 7).")
        );
    }

    #[test]
    fn real_text_is_laid_out_at_any_width_and_measured_once() {
        let long = "A long message that must wrap onto several lines when the window is narrow. "
            .repeat(8);
        let rows: Vec<Row> = (1..=12)
            .map(|n| Row {
                message: message(n, if n % 3 == 0 { "short" } else { &long }),
                revision: 1,
            })
            .collect();
        let mut totals = Vec::new();
        for width in [320.0, 640.0, 1100.0] {
            let snapshot = Snapshot {
                channel_id: Some(Snowflake(500)),
                window: Window {
                    range: 0..rows.len(),
                    height: 500.0,
                    width_bucket: variable_list::width_bucket(width),
                    at_bottom: true,
                    pinned: true,
                    ..Window::default()
                },
                rows: rows.clone(),
                ..Snapshot::default()
            };
            let mut ui = Ui::new(width, 500.0, view(&snapshot));
            let events = ui.redraw();
            ui.draw();
            let mut viewport = None;
            let mut heights = Vec::new();
            for event in events {
                match event {
                    Event::Viewport {
                        channel_id,
                        viewport: reported,
                    } => {
                        assert_eq!(channel_id, Snowflake(500));
                        viewport = Some(reported);
                    }
                    Event::Measured {
                        channel_id,
                        measurement,
                    } => {
                        assert_eq!(channel_id, Snowflake(500));
                        assert_eq!(measurement.revision, 1);
                        assert_eq!(measurement.width_bucket, variable_list::width_bucket(width));
                        assert!(measurement.height.is_finite() && measurement.height > 0.0);
                        heights.push((measurement.id, measurement.height));
                    }
                    other => panic!("unexpected event {other:?}"),
                }
            }
            let viewport = viewport.expect("the first redraw reports the viewport");
            assert_eq!((viewport.width, viewport.height), (width, 500.0));
            assert!(
                viewport.at_bottom,
                "a pinned timeline opens at the newest message"
            );
            // One height per built row, in order, reported once; an idle window is silent.
            assert_eq!(
                heights.iter().map(|(id, _)| id.0).collect::<Vec<_>>(),
                (1..=12).collect::<Vec<_>>()
            );
            assert!(ui.redraw().is_empty());
            totals.push(heights.iter().map(|(_, height)| height).sum::<f32>());
        }
        // Narrower rows wrap onto more lines, never fewer.
        assert!(
            totals[0] >= totals[1] && totals[1] >= totals[2],
            "{totals:?}"
        );
    }
}
