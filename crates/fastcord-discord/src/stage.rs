//! Stage channels: the audience, the speakers and the moderators' requests.
//!
//! Every write is one explicit user action; nothing retries or repeats. Whether
//! an action is offered is judged here from the reducer's state and again
//! immediately before sending, and Discord's own answer stays authoritative.
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};
use fastcord_model::{
    ChannelKind, GuildScope, MemberScope, Permissions, Snowflake, channel_permissions,
};
use reqwest::Method;

/// The longest Stage topic Discord accepts.
pub const MAX_TOPIC_CHARS: usize = 120;

/// One Stage write. `RequestToSpeak`, `CancelRequest`, `BecomeSpeaker` and
/// `MoveToAudience` change the account's own voice state; the rest need
/// moderator permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageAction {
    RequestToSpeak,
    CancelRequest,
    /// Accept an invitation or, with MUTE_MEMBERS, speak directly.
    BecomeSpeaker,
    MoveToAudience,
    /// Invite a member to speak, or approve their request.
    Invite(Snowflake),
    /// Move a speaker back to the audience.
    Demote(Snowflake),
    Start(String),
    SetTopic(String),
    End,
}

/// What the account may do in one Stage channel; all `false` when anything
/// needed to decide is unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StagePermissions {
    /// REQUEST_TO_SPEAK.
    pub request: bool,
    /// MUTE_MEMBERS: invite, approve and move speakers, or speak without asking.
    pub moderate: bool,
    /// Stage moderator (MANAGE_CHANNELS, MUTE_MEMBERS and MOVE_MEMBERS): may
    /// start, edit and end the session.
    pub manage: bool,
}

/// A topic Discord accepts: 1 to 120 characters after trimming.
pub fn topic(raw: &str) -> Option<&str> {
    let topic = raw.trim();
    (!topic.is_empty() && topic.chars().count() <= MAX_TOPIC_CHARS).then_some(topic)
}

impl crate::state::Store {
    /// Permissions in a Stage channel; computed on the reducer, failing closed
    /// while the guild's roles or the account's member are not known.
    pub fn stage_permissions(&self, guild_id: Snowflake, channel: Snowflake) -> StagePermissions {
        let query = || {
            let guild = self.guild(guild_id)?;
            let channel = guild
                .channel(channel)
                .filter(|c| c.kind == ChannelKind::GuildStageVoice)?;
            let me = self.current_user()?.id;
            let owner = guild.owner_id()?;
            let own = guild.member(me);
            if me != owner
                && (own.is_none_or(|m| !m.roles_known)
                    || !guild.roles().iter().any(|r| r.id == guild_id))
            {
                return None;
            }
            if me != owner
                && own.is_some_and(|m| {
                    m.communication_disabled_until
                        .as_deref()
                        .is_some_and(crate::state::navigation::active_timeout)
                })
            {
                return None;
            }
            let scope = GuildScope {
                id: guild_id,
                owner_id: owner,
                roles: guild.roles(),
            };
            let member = MemberScope {
                user_id: me,
                role_ids: own.map_or(&[][..], |m| m.roles.as_slice()),
                timed_out: false,
            };
            let all = channel_permissions(scope, member, &channel.permission_overwrites, false);
            let moderate = all.contains(Permissions::MUTE_MEMBERS);
            Some(StagePermissions {
                request: all.contains(Permissions::REQUEST_TO_SPEAK),
                moderate,
                manage: moderate
                    && all.contains(Permissions::MANAGE_CHANNELS)
                    && all.contains(Permissions::MOVE_MEMBERS),
            })
        };
        query().unwrap_or_default()
    }

    /// Whether `action` is allowed right now by permissions and by the state it
    /// changes, so a stale click or a state already reached sends nothing.
    pub fn stage_allows(
        &self,
        guild_id: Snowflake,
        channel: Snowflake,
        action: &StageAction,
    ) -> bool {
        let (Some(guild), Some(me)) = (self.guild(guild_id), self.current_user().map(|u| u.id))
        else {
            return false;
        };
        let permissions = self.stage_permissions(guild_id, channel);
        let live = guild.stage_instance(channel).is_some();
        let seat = |user: Snowflake| {
            guild
                .voice_state(user)
                .filter(|v| v.channel_id == Some(channel))
        };
        match action {
            StageAction::RequestToSpeak => {
                permissions.request
                    && seat(me)
                        .is_some_and(|v| v.suppress && v.request_to_speak_timestamp.is_none())
            }
            StageAction::CancelRequest => {
                seat(me).is_some_and(|v| v.request_to_speak_timestamp.is_some())
            }
            StageAction::BecomeSpeaker => seat(me).is_some_and(|v| {
                v.suppress && (permissions.moderate || v.request_to_speak_timestamp.is_some())
            }),
            StageAction::MoveToAudience => seat(me).is_some_and(|v| !v.suppress),
            StageAction::Invite(user) => {
                permissions.moderate && *user != me && seat(*user).is_some_and(|v| v.suppress)
            }
            StageAction::Demote(user) => {
                permissions.moderate && *user != me && seat(*user).is_some_and(|v| !v.suppress)
            }
            StageAction::Start(raw) => permissions.manage && !live && topic(raw).is_some(),
            StageAction::SetTopic(raw) => permissions.manage && live && topic(raw).is_some(),
            StageAction::End => permissions.manage && live,
        }
    }
}

type Parts = (Method, String, Option<serde_json::Value>);

fn parts(guild: Snowflake, channel: Snowflake, action: &StageAction, now: &str) -> Parts {
    let own = format!("/guilds/{guild}/voice-states/@me");
    let other = |user: Snowflake| format!("/guilds/{guild}/voice-states/{user}");
    // Every voice-state write names the Stage's channel.
    let state = |suppress: bool| serde_json::json!({ "channel_id": channel, "suppress": suppress });
    match action {
        StageAction::RequestToSpeak => (
            Method::PATCH,
            own,
            Some(serde_json::json!({ "channel_id": channel, "request_to_speak_timestamp": now })),
        ),
        StageAction::CancelRequest => (
            Method::PATCH,
            own,
            Some(serde_json::json!({ "channel_id": channel, "request_to_speak_timestamp": null })),
        ),
        StageAction::BecomeSpeaker => (Method::PATCH, own, Some(state(false))),
        StageAction::MoveToAudience => (Method::PATCH, own, Some(state(true))),
        StageAction::Invite(user) => (Method::PATCH, other(*user), Some(state(false))),
        StageAction::Demote(user) => (Method::PATCH, other(*user), Some(state(true))),
        StageAction::Start(raw) => (
            Method::POST,
            "/stage-instances".to_owned(),
            // privacy_level 2 is GUILD_ONLY, the only level Discord still accepts.
            Some(serde_json::json!({
                "channel_id": channel,
                "topic": topic(raw).unwrap_or_default(),
                "privacy_level": 2,
            })),
        ),
        StageAction::SetTopic(raw) => (
            Method::PATCH,
            format!("/stage-instances/{channel}"),
            Some(serde_json::json!({ "topic": topic(raw).unwrap_or_default() })),
        ),
        StageAction::End => (Method::DELETE, format!("/stage-instances/{channel}"), None),
    }
}

fn request(
    guild: Snowflake,
    channel: Snowflake,
    action: &StageAction,
    now: &str,
) -> Result<RestRequest, RestError> {
    let (method, path, body) = parts(guild, channel, action, now);
    let request =
        RestRequest::new(Route::new(method, &path)?, Priority::UserWrite).with_error_detail();
    match body {
        Some(body) => request.json(&body),
        None => Ok(request),
    }
}

/// `YYYY-MM-DDTHH:MM:SS.000Z` for a Unix time in seconds; empty past the
/// year 9999, which no Discord field accepts.
pub fn iso8601(unix: u64) -> String {
    let Some(t) = i64::try_from(unix)
        .ok()
        .and_then(|unix| time::OffsetDateTime::from_unix_timestamp(unix).ok())
    else {
        return String::new();
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

impl<C: Clock> RestClient<C> {
    /// One Stage write. `now` is the ISO time of a speaking request.
    pub async fn stage_action(
        &self,
        guild: Snowflake,
        channel: Snowflake,
        action: &StageAction,
        now: &str,
    ) -> Result<(), RestError> {
        self.execute(request(guild, channel, action, now)?)
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests;
