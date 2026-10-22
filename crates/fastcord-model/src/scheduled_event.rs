//! Guild scheduled events (issue #92) as the Gateway and REST describe them.
//! Text is user content and is cut to Discord's own limits on decoding; an
//! event of a status or kind this client does not know is dropped.

use serde::Deserialize;

use crate::{Snowflake, parse_unix_ms};

pub const MAX_NAME_CHARS: usize = 100;
pub const MAX_DESCRIPTION_CHARS: usize = 1000;
pub const MAX_LOCATION_CHARS: usize = 100;
const MAX_CREATOR_CHARS: usize = 40;
const MAX_RECURRENCE_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventStatus {
    Scheduled,
    Active,
    Completed,
    Canceled,
}

impl EventStatus {
    fn from_wire(raw: u8) -> Option<Self> {
        Some(match raw {
            1 => Self::Scheduled,
            2 => Self::Active,
            3 => Self::Completed,
            4 => Self::Canceled,
            _ => return None,
        })
    }

    /// The `status` a PATCH sets.
    pub const fn wire(self) -> u8 {
        match self {
            Self::Scheduled => 1,
            Self::Active => 2,
            Self::Completed => 3,
            Self::Canceled => 4,
        }
    }

    pub const fn is_over(self) -> bool {
        matches!(self, Self::Completed | Self::Canceled)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Stage,
    Voice,
    External,
}

impl EventKind {
    fn from_wire(raw: u8) -> Option<Self> {
        Some(match raw {
            1 => Self::Stage,
            2 => Self::Voice,
            3 => Self::External,
            _ => return None,
        })
    }

    /// The `entity_type` a request sends.
    pub const fn wire(self) -> u8 {
        match self {
            Self::Stage => 1,
            Self::Voice => 2,
            Self::External => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledEvent {
    pub id: Snowflake,
    pub guild_id: Snowflake,
    /// The Stage or voice channel; `None` for an external event.
    pub channel_id: Option<Snowflake>,
    pub creator_id: Option<Snowflake>,
    /// The creator's name when Discord sent the user (empty otherwise).
    pub creator: String,
    pub name: String,
    pub description: String,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
    pub status: EventStatus,
    pub kind: EventKind,
    /// Where an external event happens.
    pub location: String,
    /// Members who are interested, as last reported; `None` if never reported.
    pub user_count: Option<u32>,
    /// Whether this account marked itself interested. Never on the wire of an
    /// event: set only from the Gateway's user add/remove events.
    pub interested: Option<bool>,
    /// A short description of the repeat rule (read-only).
    pub recurrence: Option<String>,
}

impl ScheduledEvent {
    /// The channel to join, if the event has not ended and happens in one.
    pub fn joinable_channel(&self) -> Option<Snowflake> {
        self.channel_id.filter(|_| !self.status.is_over())
    }

    /// `https://discord.com/events/{guild}/{event}`.
    pub fn link(&self) -> String {
        format!("https://discord.com/events/{}/{}", self.guild_id, self.id)
    }
}

fn clip(text: String, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => text[..at].to_owned(),
        None => text,
    }
}

#[derive(Deserialize)]
struct WireCreator {
    username: Option<String>,
    global_name: Option<String>,
}

#[derive(Deserialize)]
struct WireMetadata {
    location: Option<String>,
}

#[derive(Deserialize)]
struct WireRule {
    frequency: Option<u8>,
    interval: Option<u32>,
    by_weekday: Option<Vec<u8>>,
}

#[derive(Deserialize)]
struct Wire {
    id: Snowflake,
    guild_id: Snowflake,
    channel_id: Option<Snowflake>,
    creator_id: Option<Snowflake>,
    creator: Option<WireCreator>,
    name: Option<String>,
    description: Option<String>,
    scheduled_start_time: String,
    scheduled_end_time: Option<String>,
    status: u8,
    entity_type: u8,
    entity_metadata: Option<WireMetadata>,
    user_count: Option<u32>,
    recurrence_rule: Option<WireRule>,
}

/// "Every 2 weeks on Mon, Wed" from Discord's `recurrence_rule` (0 yearly,
/// 1 monthly, 2 weekly, 3 daily; weekdays count from Monday).
fn recurrence(rule: &WireRule) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let unit = match rule.frequency {
        Some(0) => "year",
        Some(1) => "month",
        Some(2) => "week",
        Some(3) => "day",
        _ => return "Repeats".to_owned(),
    };
    let mut text = match rule.interval.unwrap_or(1) {
        0 | 1 => format!("Every {unit}"),
        n => format!("Every {n} {unit}s"),
    };
    let days: Vec<&str> = rule
        .by_weekday
        .iter()
        .flatten()
        .filter_map(|d| DAYS.get(usize::from(*d)).copied())
        .take(7)
        .collect();
    if !days.is_empty() {
        text.push_str(" on ");
        text.push_str(&days.join(", "));
    }
    clip(text, MAX_RECURRENCE_CHARS)
}

impl TryFrom<Wire> for ScheduledEvent {
    type Error = &'static str;

    fn try_from(wire: Wire) -> Result<Self, Self::Error> {
        let status = EventStatus::from_wire(wire.status).ok_or("unknown event status")?;
        let kind = EventKind::from_wire(wire.entity_type).ok_or("unknown event kind")?;
        let start_ms = parse_unix_ms(&wire.scheduled_start_time).ok_or("bad start time")?;
        Ok(Self {
            id: wire.id,
            guild_id: wire.guild_id,
            channel_id: wire.channel_id,
            creator_id: wire.creator_id,
            creator: clip(
                wire.creator
                    .and_then(|c| c.global_name.filter(|n| !n.is_empty()).or(c.username))
                    .unwrap_or_default(),
                MAX_CREATOR_CHARS,
            ),
            name: clip(wire.name.unwrap_or_default(), MAX_NAME_CHARS),
            description: clip(wire.description.unwrap_or_default(), MAX_DESCRIPTION_CHARS),
            start_ms,
            end_ms: wire.scheduled_end_time.as_deref().and_then(parse_unix_ms),
            status,
            kind,
            location: clip(
                wire.entity_metadata
                    .and_then(|m| m.location)
                    .unwrap_or_default(),
                MAX_LOCATION_CHARS,
            ),
            user_count: wire.user_count,
            interested: None,
            recurrence: wire.recurrence_rule.as_ref().map(recurrence),
        })
    }
}

impl<'de> Deserialize<'de> for ScheduledEvent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(Wire::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Which account's interest a user add/remove event reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct ScheduledEventUser {
    pub guild_id: Snowflake,
    #[serde(rename = "guild_scheduled_event_id")]
    pub event_id: Snowflake,
    pub user_id: Snowflake,
}

/// The ids of a deleted event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub struct ScheduledEventKey {
    pub guild_id: Snowflake,
    pub id: Snowflake,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wire() -> serde_json::Value {
        json!({
            "id": "900", "guild_id": "100", "channel_id": "200", "creator_id": "7",
            "creator": {"username": "host", "global_name": "Host"},
            "name": "Movie night", "description": "Bring snacks",
            "scheduled_start_time": "2026-10-30T18:00:00.000000+00:00",
            "scheduled_end_time": null, "status": 1, "entity_type": 2,
            "entity_metadata": null, "user_count": 12,
            "recurrence_rule": {"frequency": 2, "interval": 2, "by_weekday": [0, 2]}
        })
    }

    #[test]
    fn decodes_an_event() {
        let e: ScheduledEvent = serde_json::from_value(wire()).unwrap();
        assert_eq!(
            (e.status, e.kind),
            (EventStatus::Scheduled, EventKind::Voice)
        );
        assert_eq!(e.channel_id, Some(Snowflake(200)));
        assert_eq!((e.creator.as_str(), e.user_count), ("Host", Some(12)));
        assert_eq!(e.end_ms, None);
        assert_eq!(e.recurrence.as_deref(), Some("Every 2 weeks on Mon, Wed"));
        assert_eq!(e.link(), "https://discord.com/events/100/900");
        assert_eq!(e.joinable_channel(), Some(Snowflake(200)));
    }

    #[test]
    fn external_events_carry_a_location_and_cannot_be_joined() {
        let mut raw = wire();
        raw["entity_type"] = json!(3);
        raw["channel_id"] = json!(null);
        raw["entity_metadata"] = json!({"location": "Town hall"});
        raw["scheduled_end_time"] = json!("2026-10-30T20:00:00+00:00");
        let e: ScheduledEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(
            (e.kind, e.location.as_str()),
            (EventKind::External, "Town hall")
        );
        assert_eq!(e.end_ms, Some(e.start_ms + 7_200_000));
        assert_eq!(e.joinable_channel(), None);
    }

    #[test]
    fn unknown_or_broken_events_are_refused_and_text_is_cut() {
        for (field, value) in [
            ("status", json!(9)),
            ("entity_type", json!(0)),
            ("scheduled_start_time", json!("soon")),
        ] {
            let mut raw = wire();
            raw[field] = value;
            assert!(serde_json::from_value::<ScheduledEvent>(raw).is_err());
        }
        let mut raw = wire();
        raw["name"] = json!("n".repeat(500));
        raw["description"] = json!("d".repeat(5000));
        raw["status"] = json!(4);
        let e: ScheduledEvent = serde_json::from_value(raw).unwrap();
        assert_eq!(e.name.len(), MAX_NAME_CHARS);
        assert_eq!(e.description.len(), MAX_DESCRIPTION_CHARS);
        assert_eq!(e.joinable_channel(), None, "a canceled event is not joined");
    }

    #[test]
    fn recurrence_reads_plainly() {
        let rule = |frequency, interval| WireRule {
            frequency,
            interval,
            by_weekday: None,
        };
        assert_eq!(recurrence(&rule(Some(3), Some(1))), "Every day");
        assert_eq!(recurrence(&rule(Some(1), None)), "Every month");
        assert_eq!(recurrence(&rule(Some(0), Some(3))), "Every 3 years");
        assert_eq!(recurrence(&rule(None, None)), "Repeats");
    }
}
