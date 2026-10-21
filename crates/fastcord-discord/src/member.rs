//! Explicit, single-member server actions. No bulk operation or retry policy.
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};
use fastcord_model::{
    GuildScope, MemberScope, Permissions, Snowflake, channel_permissions, guild_permissions,
};
use reqwest::Method;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemberAction {
    Mute(bool),
    Deafen(bool),
    Disconnect,
    Kick,
    Ban,
}

impl MemberAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Mute(true) => "Server mute",
            Self::Mute(false) => "Server unmute",
            Self::Deafen(true) => "Server deafen",
            Self::Deafen(false) => "Server undeafen",
            Self::Disconnect => "Disconnect from voice",
            Self::Kick => "Kick from server",
            Self::Ban => "Ban from server",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MemberPermissions {
    pub kick: bool,
    pub ban: bool,
    pub mute: bool,
    pub deafen: bool,
    pub disconnect: bool,
    pub server_muted: bool,
    pub server_deafened: bool,
}

impl MemberPermissions {
    pub fn allows(self, action: MemberAction) -> bool {
        match action {
            MemberAction::Kick => self.kick,
            MemberAction::Ban => self.ban,
            MemberAction::Mute(value) => self.mute && value != self.server_muted,
            MemberAction::Deafen(value) => self.deafen && value != self.server_deafened,
            MemberAction::Disconnect => self.disconnect,
        }
    }
}

impl crate::state::Store {
    /// Current permissions, hierarchy and voice state, computed on the reducer.
    /// Missing authoritative role/member data fails closed; no member fetch.
    pub fn member_permissions(&self, guild_id: Snowflake, user: Snowflake) -> MemberPermissions {
        let query = || {
            let guild = self.guild(guild_id)?;
            let me = self.current_user()?.id;
            let owner = guild.owner_id()?;
            if me == user || owner == user {
                return None;
            }
            let target = guild.member(user)?;
            if !target.roles_known {
                return None;
            }
            let rank = |roles: &[Snowflake]| -> Option<i64> {
                let mut highest = 0;
                for id in roles {
                    let role = guild.roles().iter().find(|r| r.id == *id)?;
                    highest = highest.max(i64::from(role.position));
                }
                Some(highest)
            };
            let scope = GuildScope {
                id: guild_id,
                owner_id: owner,
                roles: guild.roles(),
            };
            let own = guild.member(me);
            if me != owner
                && (own.is_none_or(|m| !m.roles_known)
                    || !guild.roles().iter().any(|r| r.id == guild_id))
            {
                return None;
            }
            let roles = own.map_or(&[][..], |m| m.roles.as_slice());
            if me != owner && rank(roles)? <= rank(&target.roles)? {
                return None;
            }
            // A timed-out moderator cannot perform communication actions.
            let member = MemberScope {
                user_id: me,
                role_ids: roles,
                timed_out: false,
            };
            if me != owner
                && own.is_some_and(|m| {
                    m.communication_disabled_until
                        .as_deref()
                        .is_some_and(crate::state::navigation::active_timeout)
                })
            {
                return None;
            }
            let base = guild_permissions(scope, member);
            let voice = guild.voice_state(user).filter(|v| v.channel_id.is_some());
            let channel = voice
                .and_then(|v| v.channel_id)
                .and_then(|id| guild.channel(id));
            let effective = channel.map_or(Permissions::NONE, |c| {
                channel_permissions(scope, member, &c.permission_overwrites, false)
            });
            Some(MemberPermissions {
                kick: base.contains(Permissions::KICK_MEMBERS),
                ban: base.contains(Permissions::BAN_MEMBERS),
                mute: effective.contains(Permissions::MUTE_MEMBERS),
                deafen: effective.contains(Permissions::DEAFEN_MEMBERS),
                disconnect: effective.contains(Permissions::MOVE_MEMBERS),
                server_muted: voice.is_some_and(|v| v.mute),
                server_deafened: voice.is_some_and(|v| v.deaf),
            })
        };
        query().unwrap_or_default()
    }
}

type Parts = (Method, String, Option<serde_json::Value>);

fn parts(guild: Snowflake, user: Snowflake, action: MemberAction) -> Parts {
    let member = format!("/guilds/{guild}/members/{user}");
    match action {
        MemberAction::Mute(value) => (
            Method::PATCH,
            member,
            Some(serde_json::json!({ "mute": value })),
        ),
        MemberAction::Deafen(value) => (
            Method::PATCH,
            member,
            Some(serde_json::json!({ "deaf": value })),
        ),
        MemberAction::Disconnect => (
            Method::PATCH,
            member,
            Some(serde_json::json!({ "channel_id": null })),
        ),
        MemberAction::Kick => (Method::DELETE, member, None),
        // Never delete message history as an incidental effect of a ban.
        MemberAction::Ban => (
            Method::PUT,
            format!("/guilds/{guild}/bans/{user}"),
            Some(serde_json::json!({ "delete_message_seconds": 0 })),
        ),
    }
}

fn request(
    guild: Snowflake,
    user: Snowflake,
    action: MemberAction,
) -> Result<RestRequest, RestError> {
    let (method, path, body) = parts(guild, user, action);
    let request =
        RestRequest::new(Route::new(method, &path)?, Priority::UserWrite).with_error_detail();
    match body {
        Some(body) => request.json(&body),
        None => Ok(request),
    }
}

impl<C: Clock> RestClient<C> {
    pub async fn member_action(
        &self,
        guild: Snowflake,
        user: Snowflake,
        action: MemberAction,
    ) -> Result<(), RestError> {
        self.execute(request(guild, user, action)?).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(action: MemberAction) -> Option<serde_json::Value> {
        assert!(request(Snowflake(1), Snowflake(2), action).is_ok());
        parts(Snowflake(1), Snowflake(2), action).2
    }

    #[test]
    fn each_action_sends_one_minimal_body() {
        assert_eq!(
            body(MemberAction::Mute(true)),
            Some(serde_json::json!({"mute": true}))
        );
        assert_eq!(
            body(MemberAction::Deafen(false)),
            Some(serde_json::json!({"deaf": false}))
        );
        assert_eq!(
            body(MemberAction::Disconnect),
            Some(serde_json::json!({"channel_id": null}))
        );
        assert_eq!(body(MemberAction::Kick), None);
        assert_eq!(
            parts(Snowflake(1), Snowflake(2), MemberAction::Kick).0,
            Method::DELETE
        );
        assert_eq!(
            parts(Snowflake(1), Snowflake(2), MemberAction::Ban).1,
            "/guilds/1/bans/2"
        );
        // A ban never deletes message history on the side.
        assert_eq!(
            body(MemberAction::Ban),
            Some(serde_json::json!({"delete_message_seconds": 0}))
        );
    }

    #[test]
    fn toggles_are_offered_only_towards_the_other_state() {
        let p = MemberPermissions {
            mute: true,
            server_muted: true,
            ..MemberPermissions::default()
        };
        assert!(p.allows(MemberAction::Mute(false)));
        assert!(!p.allows(MemberAction::Mute(true)));
        assert!(!p.allows(MemberAction::Ban));
    }
}
