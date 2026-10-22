//! Explicit, single-member server actions. No bulk operation or retry policy.
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};
use fastcord_model::{
    GuildScope, MemberScope, Permissions, Snowflake, channel_permissions, guild_permissions,
};
use reqwest::Method;

/// Timeouts the member menu offers, in seconds (Discord allows up to 28 days).
pub const TIMEOUTS: [(u32, &str); 6] = [
    (60, "60 seconds"),
    (300, "5 minutes"),
    (600, "10 minutes"),
    (3_600, "1 hour"),
    (86_400, "1 day"),
    (604_800, "1 week"),
];

/// Message history a ban may delete, in seconds. The default is none.
pub const BAN_DELETE: [(u32, &str); 7] = [
    (0, "Keep all messages"),
    (3_600, "Last hour"),
    (21_600, "Last 6 hours"),
    (43_200, "Last 12 hours"),
    (86_400, "Last 24 hours"),
    (259_200, "Last 3 days"),
    (604_800, "Last 7 days"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemberAction {
    Mute(bool),
    Deafen(bool),
    Disconnect,
    Kick,
    /// Ban, deleting this many seconds of the member's messages (a
    /// [`BAN_DELETE`] preset; 0 keeps them).
    Ban(u32),
    /// Time out for this many seconds from now (a [`TIMEOUTS`] preset), or
    /// `None` to remove the timeout.
    Timeout(Option<u32>),
    /// Another member's nickname; `None` resets it.
    Nick(Option<String>),
    /// The account's own nickname; `None` resets it.
    OwnNick(Option<String>),
    /// Move a member who is in voice to this channel.
    Move(Snowflake),
}

impl MemberAction {
    /// A nickname change from typed text: trimmed, empty resets, at most
    /// [`MAX_NICK_CHARS`](crate::MAX_NICK_CHARS) characters.
    pub fn nick(own: bool, text: &str) -> Option<Self> {
        let text = text.trim();
        if text.chars().count() > crate::MAX_NICK_CHARS {
            return None;
        }
        let nick = (!text.is_empty()).then(|| text.to_owned());
        Some(if own {
            Self::OwnNick(nick)
        } else {
            Self::Nick(nick)
        })
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Mute(true) => "Server mute",
            Self::Mute(false) => "Server unmute",
            Self::Deafen(true) => "Server deafen",
            Self::Deafen(false) => "Server undeafen",
            Self::Disconnect => "Disconnect from voice",
            Self::Kick => "Kick from server",
            Self::Ban(_) => "Ban from server",
            Self::Timeout(Some(_)) => "Time out",
            Self::Timeout(None) => "Remove timeout",
            Self::Nick(Some(_)) | Self::OwnNick(Some(_)) => "Change nickname",
            Self::Nick(None) | Self::OwnNick(None) => "Reset nickname",
            Self::Move(_) => "Move to voice channel",
        }
    }

    /// Actions that restrict or remove someone and need an explicit Confirm.
    pub fn needs_confirm(&self) -> bool {
        matches!(
            self,
            Self::Kick | Self::Ban(_) | Self::Disconnect | Self::Timeout(Some(_))
        )
    }

    /// Whether Discord records the optional audit-log reason for it.
    pub fn takes_reason(&self) -> bool {
        matches!(
            self,
            Self::Kick | Self::Ban(_) | Self::Timeout(_) | Self::Nick(_) | Self::OwnNick(_)
        )
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
    /// MODERATE_MEMBERS over a member who is not an administrator.
    pub timeout: bool,
    /// The member is timed out now.
    pub timed_out: bool,
    /// CHANGE_NICKNAME for the account itself, MANAGE_NICKNAMES for others.
    pub nickname: bool,
}

impl MemberPermissions {
    pub fn allows(self, action: &MemberAction) -> bool {
        match action {
            MemberAction::Kick => self.kick,
            MemberAction::Ban(seconds) => self.ban && BAN_DELETE.iter().any(|(s, _)| s == seconds),
            MemberAction::Mute(value) => self.mute && *value != self.server_muted,
            MemberAction::Deafen(value) => self.deafen && *value != self.server_deafened,
            // Moving needs the same power over the member's channel.
            MemberAction::Disconnect | MemberAction::Move(_) => self.disconnect,
            MemberAction::Timeout(Some(seconds)) => {
                self.timeout && TIMEOUTS.iter().any(|(s, _)| s == seconds)
            }
            MemberAction::Timeout(None) => self.timeout && self.timed_out,
            MemberAction::Nick(_) | MemberAction::OwnNick(_) => self.nickname,
        }
    }
}

impl crate::state::Store {
    /// Current permissions, hierarchy and voice state, computed on the reducer.
    /// Missing authoritative role/member data fails closed; no member fetch.
    /// For the account itself only the nickname permission is set.
    pub fn member_permissions(&self, guild_id: Snowflake, user: Snowflake) -> MemberPermissions {
        let query = || {
            let guild = self.guild(guild_id)?;
            let me = self.current_user()?.id;
            let owner = guild.owner_id()?;
            let own = guild.member(me);
            if me != owner
                && (own.is_none_or(|m| !m.roles_known)
                    || !guild.roles().iter().any(|r| r.id == guild_id))
            {
                return None;
            }
            // A timed-out moderator cannot perform communication actions.
            if me != owner
                && own.is_some_and(|m| {
                    m.communication_disabled_until
                        .as_deref()
                        .is_some_and(crate::state::navigation::active_timeout)
                })
            {
                return None;
            }
            let roles = own.map_or(&[][..], |m| m.roles.as_slice());
            let scope = GuildScope {
                id: guild_id,
                owner_id: owner,
                roles: guild.roles(),
            };
            let member = MemberScope {
                user_id: me,
                role_ids: roles,
                timed_out: false,
            };
            let base = guild_permissions(scope, member);
            if me == user {
                return Some(MemberPermissions {
                    nickname: base.contains(Permissions::CHANGE_NICKNAME),
                    ..MemberPermissions::default()
                });
            }
            if owner == user {
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
            if me != owner && rank(roles)? <= rank(&target.roles)? {
                return None;
            }
            let theirs = guild_permissions(
                scope,
                MemberScope {
                    user_id: user,
                    role_ids: &target.roles,
                    timed_out: false,
                },
            );
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
                // Administrators cannot be timed out.
                timeout: base.contains(Permissions::MODERATE_MEMBERS)
                    && !theirs.contains(Permissions::ADMINISTRATOR),
                timed_out: target
                    .communication_disabled_until
                    .as_deref()
                    .is_some_and(crate::state::navigation::active_timeout),
                nickname: base.contains(Permissions::MANAGE_NICKNAMES),
            })
        };
        query().unwrap_or_default()
    }

    /// Whether `user`, now in voice, may be moved to `dest`: a voice channel
    /// both the account and the member can see and connect to.
    pub fn can_move_to(&self, guild_id: Snowflake, user: Snowflake, dest: Snowflake) -> bool {
        let query = || {
            let guild = self.guild(guild_id)?;
            let me = self.current_user()?.id;
            let owner = guild.owner_id()?;
            let current = guild.voice_state(user).and_then(|v| v.channel_id)?;
            let channel = guild.channel(dest).filter(|c| c.kind.is_voice())?;
            let (own, target) = (guild.member(me), guild.member(user)?);
            let scope = GuildScope {
                id: guild_id,
                owner_id: owner,
                roles: guild.roles(),
            };
            let need = Permissions::VIEW_CHANNEL | Permissions::CONNECT;
            let can = |user_id, roles: &[Snowflake]| {
                channel_permissions(
                    scope,
                    MemberScope {
                        user_id,
                        role_ids: roles,
                        timed_out: false,
                    },
                    &channel.permission_overwrites,
                    false,
                )
                .contains(need)
            };
            let mine = own.map_or(&[][..], |m| m.roles.as_slice());
            Some(current != dest && can(me, mine) && can(user, &target.roles))
        };
        query().unwrap_or(false)
    }

    /// The reducer's last check before a member action is sent: current
    /// permissions and hierarchy, plus the destination of a move and the
    /// self/other split of nickname changes.
    pub fn member_action_allowed(
        &self,
        guild_id: Snowflake,
        user: Snowflake,
        action: &MemberAction,
    ) -> bool {
        let own = self.current_user().map(|me| me.id) == Some(user);
        let permitted = self.member_permissions(guild_id, user).allows(action);
        match action {
            MemberAction::OwnNick(_) => own && permitted,
            MemberAction::Nick(_) => !own && permitted,
            MemberAction::Move(dest) => permitted && self.can_move_to(guild_id, user, *dest),
            _ => !own && permitted,
        }
    }
}

type Parts = (Method, String, Option<serde_json::Value>);

/// `now` is the Unix time in seconds that a timeout counts from.
fn parts(guild: Snowflake, user: Snowflake, action: &MemberAction, now: u64) -> Parts {
    let member = format!("/guilds/{guild}/members/{user}");
    let patch = |body| (Method::PATCH, member.clone(), Some(body));
    match action {
        MemberAction::Mute(value) => patch(serde_json::json!({ "mute": value })),
        MemberAction::Deafen(value) => patch(serde_json::json!({ "deaf": value })),
        MemberAction::Disconnect => patch(serde_json::json!({ "channel_id": null })),
        MemberAction::Move(channel) => patch(serde_json::json!({ "channel_id": channel })),
        MemberAction::Timeout(until) => patch(serde_json::json!({
            "communication_disabled_until":
                until.map(|seconds| crate::iso8601(now + u64::from(seconds)))
        })),
        MemberAction::Nick(nick) => patch(serde_json::json!({ "nick": nick })),
        MemberAction::OwnNick(nick) => (
            Method::PATCH,
            format!("/guilds/{guild}/members/@me"),
            Some(serde_json::json!({ "nick": nick })),
        ),
        MemberAction::Kick => (Method::DELETE, member, None),
        // History is deleted only when the user picked a window for it.
        MemberAction::Ban(seconds) => (
            Method::PUT,
            format!("/guilds/{guild}/bans/{user}"),
            Some(serde_json::json!({ "delete_message_seconds": seconds })),
        ),
    }
}

fn request(
    guild: Snowflake,
    user: Snowflake,
    action: &MemberAction,
    reason: Option<&str>,
    now: u64,
) -> Result<RestRequest, RestError> {
    let (method, path, body) = parts(guild, user, action, now);
    let mut request =
        RestRequest::new(Route::new(method, &path)?, Priority::UserWrite).with_error_detail();
    if let Some(reason) = reason.filter(|_| action.takes_reason()) {
        request = request.with_audit_reason(reason);
    }
    match body {
        Some(body) => request.json(&body),
        None => Ok(request),
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl<C: Clock> RestClient<C> {
    /// One explicit action on one member; `reason` goes to the audit log for
    /// the actions that take one.
    pub async fn member_action(
        &self,
        guild: Snowflake,
        user: Snowflake,
        action: &MemberAction,
        reason: Option<&str>,
    ) -> Result<(), RestError> {
        self.execute(request(guild, user, action, reason, unix_now())?)
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn body(action: MemberAction) -> Option<serde_json::Value> {
        assert!(request(Snowflake(1), Snowflake(2), &action, None, NOW).is_ok());
        parts(Snowflake(1), Snowflake(2), &action, NOW).2
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
            parts(Snowflake(1), Snowflake(2), &MemberAction::Kick, NOW).0,
            Method::DELETE
        );
        assert_eq!(
            parts(Snowflake(1), Snowflake(2), &MemberAction::Ban(0), NOW).1,
            "/guilds/1/bans/2"
        );
        // A ban deletes no history unless a window was chosen.
        assert_eq!(
            body(MemberAction::Ban(0)),
            Some(serde_json::json!({"delete_message_seconds": 0}))
        );
        assert_eq!(
            body(MemberAction::Ban(604_800)),
            Some(serde_json::json!({"delete_message_seconds": 604_800}))
        );
    }

    #[test]
    fn timeout_nickname_and_move_bodies_and_routes() {
        let until = |seconds| crate::iso8601(NOW + seconds);
        assert_eq!(
            body(MemberAction::Timeout(Some(3_600))),
            Some(serde_json::json!({"communication_disabled_until": until(3_600)}))
        );
        assert_eq!(
            body(MemberAction::Timeout(None)),
            Some(serde_json::json!({"communication_disabled_until": null}))
        );
        assert_eq!(
            body(MemberAction::Move(Snowflake(77))),
            Some(serde_json::json!({"channel_id": "77"}))
        );
        assert_eq!(
            body(MemberAction::Nick(Some("Sam".into()))),
            Some(serde_json::json!({"nick": "Sam"}))
        );
        assert_eq!(
            body(MemberAction::OwnNick(None)),
            Some(serde_json::json!({"nick": null}))
        );
        let own = parts(
            Snowflake(1),
            Snowflake(2),
            &MemberAction::OwnNick(None),
            NOW,
        );
        assert_eq!(
            (own.0, own.1.as_str()),
            (Method::PATCH, "/guilds/1/members/@me")
        );
        let other = parts(Snowflake(1), Snowflake(2), &MemberAction::Nick(None), NOW);
        assert_eq!(other.1, "/guilds/1/members/2");
    }

    #[test]
    fn nicknames_are_trimmed_bounded_and_empty_resets() {
        assert_eq!(
            MemberAction::nick(true, "  "),
            Some(MemberAction::OwnNick(None))
        );
        assert_eq!(
            MemberAction::nick(false, " Sam "),
            Some(MemberAction::Nick(Some("Sam".into())))
        );
        assert_eq!(MemberAction::nick(false, &"x".repeat(33)), None);
    }

    #[test]
    fn only_restrictive_actions_ask_for_confirm_and_reasons_are_limited() {
        for action in [
            MemberAction::Kick,
            MemberAction::Ban(0),
            MemberAction::Disconnect,
            MemberAction::Timeout(Some(60)),
        ] {
            assert!(action.needs_confirm());
        }
        assert!(!MemberAction::Timeout(None).needs_confirm());
        assert!(!MemberAction::Move(Snowflake(1)).needs_confirm());
        assert!(!MemberAction::Mute(true).takes_reason());
    }

    #[test]
    fn toggles_are_offered_only_towards_the_other_state() {
        let p = MemberPermissions {
            mute: true,
            server_muted: true,
            ..MemberPermissions::default()
        };
        assert!(p.allows(&MemberAction::Mute(false)));
        assert!(!p.allows(&MemberAction::Mute(true)));
        assert!(!p.allows(&MemberAction::Ban(0)));
    }

    #[test]
    fn presets_and_current_state_gate_timeout_and_ban() {
        let p = MemberPermissions {
            timeout: true,
            ban: true,
            ..MemberPermissions::default()
        };
        assert!(p.allows(&MemberAction::Timeout(Some(604_800))));
        assert!(!p.allows(&MemberAction::Timeout(Some(61))));
        // Nothing to remove while the member is not timed out.
        assert!(!p.allows(&MemberAction::Timeout(None)));
        assert!(
            MemberPermissions {
                timed_out: true,
                ..p
            }
            .allows(&MemberAction::Timeout(None))
        );
        assert!(p.allows(&MemberAction::Ban(259_200)));
        assert!(!p.allows(&MemberAction::Ban(5)));
    }

    fn send(store: &mut crate::state::Store, name: &str, value: serde_json::Value) {
        let event = crate::gateway::decode_fixture(name, &value.to_string());
        store.apply(crate::gateway::GatewayEvent::Dispatch { sequence: 1, event });
    }

    fn user(id: u64) -> serde_json::Value {
        serde_json::json!({"id": id.to_string(), "username": format!("u{id}")})
    }

    /// Server 100 owned by user 2. The account (1) holds the moderator role
    /// 101; 5 is below it and in voice, 6 an administrator, 7 holds an equal role.
    fn store() -> crate::state::Store {
        let p = |bits: &[Permissions]| bits.iter().fold(0, |all, p| all | p.0).to_string();
        let moderator = p(&[
            Permissions::KICK_MEMBERS,
            Permissions::BAN_MEMBERS,
            Permissions::MOVE_MEMBERS,
            Permissions::MANAGE_NICKNAMES,
            Permissions::CHANGE_NICKNAME,
            Permissions::MODERATE_MEMBERS,
        ]);
        let basic = p(&[Permissions::VIEW_CHANNEL, Permissions::CONNECT]);
        let ready = serde_json::json!({
            "session_id": "s", "resume_gateway_url": "wss://example.invalid",
            "user": user(1),
            "guilds": [{
                "id": "100", "name": "G", "owner_id": "2", "member_count": 4,
                "roles": [
                    {"id": "100", "name": "@everyone", "permissions": basic, "position": 0},
                    {"id": "101", "name": "mod", "permissions": moderator, "position": 3},
                    {"id": "102", "name": "member", "permissions": "0", "position": 1},
                    {"id": "103", "name": "admin", "permissions": p(&[Permissions::ADMINISTRATOR]), "position": 1},
                    {"id": "104", "name": "peer", "permissions": "0", "position": 3},
                ],
                "channels": [
                    {"id": "1001", "type": 2, "name": "a", "permission_overwrites": []},
                    {"id": "1002", "type": 2, "name": "b", "permission_overwrites": []},
                    {"id": "1003", "type": 2, "name": "locked", "permission_overwrites": [
                        {"id": "102", "type": 0, "allow": "0", "deny": Permissions::CONNECT.0.to_string()}
                    ]},
                    {"id": "1004", "type": 0, "name": "text", "permission_overwrites": []},
                ],
            }],
            "merged_members": [[{"user_id": "1", "roles": ["101"]}]],
        });
        let mut store = crate::state::Store::new();
        send(&mut store, "READY", ready);
        for (id, role) in [(5, "102"), (6, "103"), (7, "104")] {
            send(
                &mut store,
                "GUILD_MEMBER_ADD",
                serde_json::json!({"guild_id": "100", "user": user(id), "roles": [role]}),
            );
        }
        send(
            &mut store,
            "VOICE_STATE_UPDATE",
            serde_json::json!({
                "guild_id": "100", "channel_id": "1001", "user_id": "5", "session_id": "v",
                "deaf": false, "mute": false, "self_deaf": false, "self_mute": false,
                "member": {"user": user(5), "roles": ["102"]}
            }),
        );
        store
    }

    #[test]
    fn the_reducer_computes_timeout_nickname_and_hierarchy() {
        let (g, store) = (Snowflake(100), store());
        let target = store.member_permissions(g, Snowflake(5));
        assert!(target.kick && target.ban && target.timeout && target.nickname);
        assert!(target.disconnect && !target.timed_out);
        assert!(target.allows(&MemberAction::Timeout(Some(60))));
        assert!(!target.allows(&MemberAction::Timeout(None)));
        // An administrator can be kicked but not timed out.
        let admin = store.member_permissions(g, Snowflake(6));
        assert!(admin.kick && !admin.timeout);
        // Equal rank and the owner are out of reach; the account itself may
        // only change its own nickname.
        for user in [7, 2] {
            assert_eq!(
                store.member_permissions(g, Snowflake(user)),
                MemberPermissions::default()
            );
        }
        let own = store.member_permissions(g, Snowflake(1));
        assert!(own.nickname && !own.kick && !own.timeout);
        // Unknown servers and members fail closed.
        assert_eq!(
            store.member_permissions(Snowflake(9), Snowflake(5)),
            MemberPermissions::default()
        );
    }

    #[test]
    fn a_timed_out_member_can_have_it_removed_and_a_timed_out_moderator_cannot_act() {
        let (g, mut store) = (Snowflake(100), store());
        let update = |id: u64| {
            serde_json::json!({
                "guild_id": "100", "user": user(id),
                "roles": [if id == 1 { "101" } else { "102" }],
                "communication_disabled_until": "2099-01-01T00:00:00.000000+00:00"
            })
        };
        send(&mut store, "GUILD_MEMBER_UPDATE", update(5));
        let target = store.member_permissions(g, Snowflake(5));
        assert!(target.timed_out && target.allows(&MemberAction::Timeout(None)));
        send(&mut store, "GUILD_MEMBER_UPDATE", update(1));
        assert_eq!(
            store.member_permissions(g, Snowflake(5)),
            MemberPermissions::default()
        );
    }

    #[test]
    fn a_move_needs_a_different_voice_channel_both_can_connect_to() {
        let (g, store) = (Snowflake(100), store());
        let (member, other) = (Snowflake(5), Snowflake(1002));
        assert!(store.can_move_to(g, member, other));
        assert!(
            !store.can_move_to(g, member, Snowflake(1001)),
            "same channel"
        );
        assert!(
            !store.can_move_to(g, member, Snowflake(1003)),
            "member cannot connect"
        );
        assert!(
            !store.can_move_to(g, member, Snowflake(1004)),
            "not a voice channel"
        );
        assert!(!store.can_move_to(g, Snowflake(6), other), "not in voice");
        assert!(store.member_action_allowed(g, member, &MemberAction::Move(other)));
        assert!(!store.member_action_allowed(g, member, &MemberAction::Move(Snowflake(1003))));
    }

    #[test]
    fn nickname_actions_split_between_self_and_others() {
        let (g, store) = (Snowflake(100), store());
        let (me, other) = (Snowflake(1), Snowflake(5));
        let own = MemberAction::OwnNick(None);
        let theirs = MemberAction::Nick(Some("x".into()));
        assert!(store.member_action_allowed(g, me, &own));
        assert!(!store.member_action_allowed(g, me, &theirs));
        assert!(store.member_action_allowed(g, other, &theirs));
        assert!(
            !store.member_action_allowed(g, other, &own),
            "the own route names the account"
        );
        assert!(!store.member_action_allowed(g, me, &MemberAction::Kick));
        assert!(store.member_action_allowed(g, other, &MemberAction::Ban(86_400)));
    }
}
