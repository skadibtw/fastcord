//! Scheduled community events (SPEC §16, issue #92): listing them, marking
//! interest, and creating, editing, starting, ending, cancelling and deleting.
//! Every write is one explicit click and is never retried; whether it is
//! offered is judged from the reducer's state and again immediately before
//! sending, and Discord's own answer stays authoritative. What the account
//! sees change comes only from the Gateway. All routes are user-client routes
//! unverified against a live account (`docs/PROTOCOL.md`).

use fastcord_model::{
    EventKind, EventStatus, MAX_DESCRIPTION_CHARS, MAX_LOCATION_CHARS, MAX_NAME_CHARS, Permissions,
    ScheduledEvent, Snowflake, guild_permissions,
};
use reqwest::Method;
use serde_json::{Value, json};

use crate::state::MAX_EVENTS;
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route, iso8601};

/// A list body larger than this is not parsed at all.
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// What the user typed for a created or edited event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventDraft {
    pub name: String,
    pub description: String,
    pub kind: EventKind,
    /// The Stage or voice channel; ignored for an external event.
    pub channel: Option<Snowflake>,
    /// Where an external event happens; ignored otherwise.
    pub location: String,
    pub start_ms: u64,
    pub end_ms: Option<u64>,
}

/// Why a draft is not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftError {
    Name,
    Description,
    Channel,
    Location,
    EndRequired,
    Start,
    End,
}

impl DraftError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Name => "The name needs 1 to 100 characters.",
            Self::Description => "The description is limited to 1000 characters.",
            Self::Channel => "Choose the voice or Stage channel.",
            Self::Location => "An external event needs a location of up to 100 characters.",
            Self::EndRequired => "An external event needs an end time.",
            Self::Start => "The start time must be in the future.",
            Self::End => "The end time must be after the start.",
        }
    }
}

impl EventDraft {
    /// `future` also requires the start to lie after `now_ms` (a new event, or
    /// an edit that moves the start).
    pub fn check(&self, now_ms: u64, future: bool) -> Result<(), DraftError> {
        let chars = |text: &str| text.trim().chars().count();
        if !(1..=MAX_NAME_CHARS).contains(&chars(&self.name)) {
            return Err(DraftError::Name);
        }
        if chars(&self.description) > MAX_DESCRIPTION_CHARS {
            return Err(DraftError::Description);
        }
        match self.kind {
            EventKind::External => {
                if !(1..=MAX_LOCATION_CHARS).contains(&chars(&self.location)) {
                    return Err(DraftError::Location);
                }
                if self.end_ms.is_none() {
                    return Err(DraftError::EndRequired);
                }
            }
            EventKind::Stage | EventKind::Voice => {
                if self.channel.is_none() {
                    return Err(DraftError::Channel);
                }
            }
        }
        if future && self.start_ms <= now_ms {
            return Err(DraftError::Start);
        }
        if self.end_ms.is_some_and(|end| end <= self.start_ms) {
            return Err(DraftError::End);
        }
        Ok(())
    }

    /// The request body; a created event leaves out what is empty, an edit
    /// clears it.
    fn body(&self, edit: bool) -> Value {
        let external = self.kind == EventKind::External;
        let mut body = json!({
            "name": self.name.trim(),
            "scheduled_start_time": iso8601(self.start_ms / 1000),
            "entity_type": self.kind.wire(),
            // GUILD_ONLY, the only level Discord accepts.
            "privacy_level": 2,
        });
        let object = body.as_object_mut().expect("an object");
        let description = self.description.trim();
        if !description.is_empty() {
            object.insert("description".into(), json!(description));
        } else if edit {
            object.insert("description".into(), Value::Null);
        }
        match self.end_ms {
            Some(end) => {
                object.insert("scheduled_end_time".into(), json!(iso8601(end / 1000)));
            }
            None if edit => {
                object.insert("scheduled_end_time".into(), Value::Null);
            }
            None => {}
        }
        if external {
            object.insert(
                "entity_metadata".into(),
                json!({ "location": self.location.trim() }),
            );
            if edit {
                object.insert("channel_id".into(), Value::Null);
            }
        } else {
            object.insert("channel_id".into(), json!(self.channel));
            if edit {
                object.insert("entity_metadata".into(), Value::Null);
            }
        }
        body
    }
}

/// One write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventAction {
    Interested(Snowflake),
    NotInterested(Snowflake),
    Create(EventDraft),
    Edit(Snowflake, EventDraft),
    Start(Snowflake),
    End(Snowflake),
    Cancel(Snowflake),
    Delete(Snowflake),
}

/// What the account may do with events in one guild; all `false` when
/// anything needed to decide is unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EventPermissions {
    /// MANAGE_EVENTS: every event.
    pub manage: bool,
    /// CREATE_EVENTS: create events and manage one's own.
    pub create: bool,
}

impl crate::state::Store {
    /// Guild-level event permissions, computed on the reducer and failing
    /// closed while the guild's roles or the account's member are not known.
    pub fn event_permissions(&self, guild_id: Snowflake) -> EventPermissions {
        let query = || {
            let own = self.own_scope(guild_id)?;
            let (scope, member) = (own.guild, own.member);
            let all = guild_permissions(scope, member);
            Some(EventPermissions {
                manage: all.contains(Permissions::MANAGE_EVENTS),
                create: all.contains(Permissions::CREATE_EVENTS),
            })
        };
        query().unwrap_or_default()
    }

    /// Whether `action` is allowed right now by permissions and by the state
    /// it changes, so a stale click (a deleted, ended or already changed
    /// event, or a role that was taken away) sends nothing.
    pub fn event_allows(&self, guild_id: Snowflake, action: &EventAction) -> bool {
        let (Some(guild), Some(me)) = (self.guild(guild_id), self.current_user().map(|u| u.id))
        else {
            return false;
        };
        let permissions = self.event_permissions(guild_id);
        let held = |id: Snowflake| guild.scheduled_events().iter().find(|e| e.id == id);
        let owned = |id: Snowflake| {
            held(id)
                .filter(|e| permissions.manage || (permissions.create && e.creator_id == Some(me)))
        };
        match action {
            EventAction::Interested(id) => {
                held(*id).is_some_and(|e| !e.status.is_over() && e.interested != Some(true))
            }
            EventAction::NotInterested(id) => {
                held(*id).is_some_and(|e| e.interested != Some(false))
            }
            EventAction::Create(draft) => {
                (permissions.manage || permissions.create)
                    && draft.channel.is_none_or(|c| guild.channel(c).is_some())
            }
            EventAction::Edit(id, draft) => {
                owned(*id).is_some_and(|e| !e.status.is_over())
                    && draft.channel.is_none_or(|c| guild.channel(c).is_some())
            }
            EventAction::Start(id) => {
                owned(*id).is_some_and(|e| e.status == EventStatus::Scheduled)
            }
            EventAction::End(id) => owned(*id).is_some_and(|e| e.status == EventStatus::Active),
            EventAction::Cancel(id) => {
                owned(*id).is_some_and(|e| e.status == EventStatus::Scheduled)
            }
            EventAction::Delete(id) => owned(*id).is_some(),
        }
    }
}

type Parts = (Method, String, Option<Value>);

fn parts(guild: Snowflake, action: &EventAction) -> Parts {
    let base = format!("/guilds/{guild}/scheduled-events");
    let one = |id: &Snowflake| format!("{base}/{id}");
    let status = |id: &Snowflake, to: EventStatus| {
        (Method::PATCH, one(id), Some(json!({ "status": to.wire() })))
    };
    match action {
        EventAction::Interested(id) => (Method::PUT, format!("{}/users/@me", one(id)), None),
        EventAction::NotInterested(id) => (Method::DELETE, format!("{}/users/@me", one(id)), None),
        EventAction::Create(draft) => (Method::POST, base, Some(draft.body(false))),
        EventAction::Edit(id, draft) => (Method::PATCH, one(id), Some(draft.body(true))),
        EventAction::Start(id) => status(id, EventStatus::Active),
        EventAction::End(id) => status(id, EventStatus::Completed),
        EventAction::Cancel(id) => status(id, EventStatus::Canceled),
        EventAction::Delete(id) => (Method::DELETE, one(id), None),
    }
}

fn request(guild: Snowflake, action: &EventAction) -> Result<RestRequest, RestError> {
    let (method, path, body) = parts(guild, action);
    let request =
        RestRequest::new(Route::new(method, &path)?, Priority::UserWrite).with_error_detail();
    match body {
        Some(body) => request.json(&body),
        None => Ok(request),
    }
}

/// The events of a list answer; one that does not decode is skipped.
pub fn parse_list(body: &[u8]) -> Option<Vec<ScheduledEvent>> {
    if body.len() > MAX_BODY_BYTES {
        return None;
    }
    let raw: Vec<Value> = serde_json::from_slice(body).ok()?;
    Some(
        raw.into_iter()
            .take(MAX_EVENTS)
            .filter_map(|value| serde_json::from_value(value).ok())
            .collect(),
    )
}

impl<C: Clock> RestClient<C> {
    /// The server's events with their interested counts
    /// (`GET /guilds/{id}/scheduled-events?with_user_count=true`).
    pub async fn scheduled_events(
        &self,
        guild: Snowflake,
    ) -> Result<Vec<ScheduledEvent>, RestError> {
        let route = Route::new(
            Method::GET,
            &format!("/guilds/{guild}/scheduled-events?with_user_count=true"),
        )?;
        let response = self
            .execute(RestRequest::new(route, Priority::UserRead).with_error_detail())
            .await?;
        parse_list(response.body()).ok_or(RestError::InvalidJson)
    }

    /// One event write; sent once, never retried.
    pub async fn scheduled_event_action(
        &self,
        guild: Snowflake,
        action: &EventAction,
    ) -> Result<(), RestError> {
        self.execute(request(guild, action)?).await.map(drop)
    }
}

#[cfg(test)]
mod tests;
