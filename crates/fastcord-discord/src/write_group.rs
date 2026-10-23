//! The write allowlist. Every non-GET request is classified into a
//! [`WriteGroup`] by method, route template and, for message creation, the JSON
//! body. A group that is not enabled on the client fails before dispatch with
//! `RestError::WriteNotVerified`, so an unverified route sends nothing.
//!
//! Only the groups the maintainer has seen work on a live account are on by
//! default ([`WriteGroup::default_on`]). Anything this table does not know is
//! [`WriteGroup::Unclassified`], which is off. Voice (Gateway op 4) is not REST.
//! Password login and QR remote auth use their own HTTP path and are not covered.

use reqwest::Method;
use serde_json::Value;

macro_rules! groups {
    ($($variant:ident, $on:expr, $name:expr, $routes:expr;)*) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum WriteGroup { $($variant),* }

        impl WriteGroup {
            /// Every group, in display order.
            pub const ALL: &'static [WriteGroup] = &[$(WriteGroup::$variant),*];

            /// Enabled on a fresh client (verified on a live account).
            pub const fn default_on(self) -> bool {
                match self { $(WriteGroup::$variant => $on),* }
            }

            pub const fn name(self) -> &'static str {
                match self { $(WriteGroup::$variant => $name),* }
            }

            /// The routes the group covers, for the test switch.
            pub const fn routes(self) -> &'static str {
                match self { $(WriteGroup::$variant => $routes),* }
            }
        }
    };
}

groups! {
    BasicMessages, true, "Basic messages",
        "POST /channels/{c}/messages (plain text, replies, silent), PATCH and DELETE /channels/{c}/messages/{m}";
    OwnReactions, true, "Own reactions",
        "PUT and DELETE /channels/{c}/messages/{m}/reactions/{emoji}/@me";
    Typing, true, "Typing", "POST /channels/{c}/typing";
    Ack, true, "Read acknowledgement", "POST /channels/{c}/messages/{m}/ack";
    AttachmentRefresh, true, "Attachment URL refresh", "POST /attachments/refresh-urls";
    MessageExtras, false, "Message extras",
        "POST /channels/{c}/messages carrying a poll, stickers, a forward or TTS";
    Uploads, false, "File uploads",
        "POST /channels/{c}/attachments, and messages that reference uploaded files";
    Pins, false, "Pins", "PUT and DELETE /channels/{c}/pins/{m}";
    PollVotes, false, "Poll votes",
        "PUT /channels/{c}/polls/{m}/answers/@me, POST /channels/{c}/polls/{m}/expire";
    Interactions, false, "Application interactions", "POST /interactions";
    CallRing, false, "Ring a call",
        "POST /channels/{c}/call/ring and /stop-ringing";
    SoundboardPlay, false, "Soundboard play", "POST /channels/{c}/send-soundboard-sound";
    OpenDm, false, "Open a DM by typed ID", "POST /users/@me/channels";
    GroupDm, false, "Group DM management",
        "PUT and DELETE /channels/{c}/recipients/{u}, PATCH and DELETE /channels/{c} of a group DM";
    Relationships, false, "Relationships, ignore and message requests",
        "PUT and DELETE /users/@me/relationships/{u}[/ignore], PUT /channels/{c}/recipients/@me, DELETE /channels/{c} (decline)";
    FriendRequest, false, "Friend request by username", "POST /users/@me/relationships";
    InviteJoin, false, "Join or leave a server",
        "POST /invites/{code}, DELETE /users/@me/guilds/{g}";
    InviteManage, false, "Create and revoke invites",
        "POST /channels/{c}/invites, DELETE /invites/{code}";
    Profile, false, "Profile and connections",
        "PATCH /users/@me, /users/@me/profile, /users/@me/connections/{type}/{id}, /guilds/{g}/members/@me";
    UserSettings, false, "Status and privacy settings",
        "PATCH /users/@me/settings, /users/@me/guilds/{g}/settings";
    SessionsLogout, false, "Log out sessions", "POST /auth/sessions/logout";
    OAuthDeauthorize, false, "Deauthorize an app", "DELETE /oauth2/tokens/{id}";
    Reports, false, "Reports", "POST /reporting/{kind}";
    ServerAdmin, false, "Server administration",
        "POST /guilds, PATCH and DELETE /guilds/{g}, POST /guilds/{g}/mfa";
    Channels, false, "Channels",
        "POST and PATCH /guilds/{g}/channels, PATCH and DELETE /channels/{c}";
    Roles, false, "Roles and permission overwrites",
        "/guilds/{g}/roles[/{r}], PUT and DELETE /guilds/{g}/members/{u}/roles/{r}, PUT and DELETE /channels/{c}/permissions/{id}";
    Moderation, false, "Moderation",
        "PATCH and DELETE /guilds/{g}/members/{u}, PUT and DELETE /guilds/{g}/bans/{u}, POST /channels/{c}/messages/bulk-delete, DELETE of others' reactions";
    AutoMod, false, "AutoMod", "POST, PATCH and DELETE /guilds/{g}/auto-moderation/rules[/{id}]";
    CommunityConfig, false, "Community and onboarding setup",
        "PATCH /guilds/{g} (Community), PATCH /guilds/{g}/member-verification, PUT /guilds/{g}/onboarding, PUT /guilds/{g}/new-member-welcome";
    Onboarding, false, "Accept rules and onboarding answers",
        "PUT /guilds/{g}/requests/@me, PUT /guilds/{g}/onboarding-responses";
    ScheduledEvents, false, "Scheduled events",
        "POST, PATCH and DELETE /guilds/{g}/scheduled-events[/{e}], PUT and DELETE .../users/@me";
    Announcements, false, "Announcements",
        "POST /channels/{c}/messages/{m}/crosspost, POST /channels/{c}/followers, DELETE /webhooks/{id} (unfollow)";
    Expressions, false, "Emoji, stickers and sounds",
        "POST, PATCH and DELETE /guilds/{g}/emojis, /stickers, /soundboard-sounds";
    Webhooks, false, "Webhooks and integrations",
        "POST /channels/{c}/webhooks, PATCH and DELETE /webhooks/{id}, DELETE /guilds/{g}/integrations/{id}";
    Stage, false, "Stage voice states and instances",
        "PATCH /guilds/{g}/voice-states/{@me|u}, POST, PATCH and DELETE /stage-instances";
    Threads, false, "Threads and forum posts",
        "POST /channels/{c}/threads, POST /channels/{c}/messages/{m}/threads, PUT and DELETE /channels/{t}/thread-members/@me, PATCH /channels/{t}";
    Unclassified, false, "Other, unclassified writes", "Any write route this table does not list";
}

impl WriteGroup {
    const fn bit(self) -> u64 {
        1 << (self as u32)
    }

    /// The enabled set of a fresh client.
    pub(crate) fn default_mask() -> u64 {
        Self::ALL
            .iter()
            .filter(|g| g.default_on())
            .fold(0, |mask, g| mask | g.bit())
    }

    pub(crate) fn in_mask(self, mask: u64) -> bool {
        mask & self.bit() != 0
    }

    pub(crate) fn set_in(self, mask: u64, on: bool) -> u64 {
        if on {
            mask | self.bit()
        } else {
            mask & !self.bit()
        }
    }

    /// Classifies a request by method, normalized route (`RouteKey::normalized`)
    /// and JSON body. Whatever is not listed is [`WriteGroup::Unclassified`],
    /// which is off. Routes shared by several features (`PATCH /channels/{c}`,
    /// `PATCH /guilds/{g}`, `DELETE /webhooks/{id}`) get their group from the
    /// caller via `RestRequest::in_group`; this is the default for the shared path.
    pub fn classify(method: &Method, normalized: &str, body: Option<&[u8]>) -> Self {
        let segments: Vec<&str> = normalized.split('/').skip(1).collect();
        match (method.as_str(), segments.as_slice()) {
            ("POST", ["attachments", "refresh-urls"]) => Self::AttachmentRefresh,
            ("POST", ["channels", _, "messages"]) => message_group(body),
            ("PATCH" | "DELETE", ["channels", _, "messages", _]) => Self::BasicMessages,
            ("POST", ["channels", _, "messages", "bulk-delete"]) => Self::Moderation,
            ("POST", ["channels", _, "messages", _, "ack"]) => Self::Ack,
            ("POST", ["channels", _, "messages", _, "crosspost"]) => Self::Announcements,
            ("POST", ["channels", _, "messages", _, "threads"] | ["channels", _, "threads"])
            | ("PUT" | "DELETE", ["channels", _, "thread-members", "@me"]) => Self::Threads,
            ("POST", ["channels", _, "typing"]) => Self::Typing,
            ("PUT", ["channels", _, "messages", _, "reactions", _, "@me"])
            | ("DELETE", ["channels", _, "messages", _, "reactions", _, .., "@me"]) => {
                Self::OwnReactions
            }
            ("DELETE", ["channels", _, "messages", _, "reactions", ..]) => Self::Moderation,
            ("POST", ["channels", _, "attachments"]) => Self::Uploads,
            ("PUT" | "DELETE", ["channels", _, "pins", _]) => Self::Pins,
            ("PUT", ["channels", _, "polls", _, "answers", "@me"])
            | ("POST", ["channels", _, "polls", _, "expire"]) => Self::PollVotes,
            ("POST", ["interactions"]) => Self::Interactions,
            ("POST", ["channels", _, "call", "ring" | "stop-ringing"]) => Self::CallRing,
            ("POST", ["channels", _, "send-soundboard-sound"]) => Self::SoundboardPlay,
            ("POST", ["users", "@me", "channels"]) => Self::OpenDm,
            ("PUT" | "DELETE", ["channels", _, "recipients", "@me"]) => Self::Relationships,
            ("PUT" | "DELETE", ["channels", _, "recipients", _]) => Self::GroupDm,
            ("PATCH" | "DELETE", ["channels", _]) => Self::Channels,
            ("POST", ["channels", _, "invites"]) => Self::InviteManage,
            ("POST", ["channels", _, "webhooks"]) => Self::Webhooks,
            ("POST", ["channels", _, "followers"]) => Self::Announcements,
            ("PUT" | "DELETE", ["channels", _, "permissions", _]) => Self::Roles,
            ("POST", ["users", "@me", "relationships"]) => Self::FriendRequest,
            ("PUT" | "DELETE", ["users", "@me", "relationships", _, ..]) => Self::Relationships,
            (
                "PATCH",
                ["users", "@me"]
                | ["users", "@me", "profile"]
                | ["users", "@me", "connections", _, _],
            ) => Self::Profile,
            ("PATCH", ["users", "@me", "settings"] | ["users", "@me", "guilds", _, "settings"]) => {
                Self::UserSettings
            }
            ("DELETE", ["users", "@me", "guilds", _]) | ("POST", ["invites", _]) => {
                Self::InviteJoin
            }
            ("DELETE", ["invites", _]) => Self::InviteManage,
            ("POST", ["auth", "sessions", "logout"]) => Self::SessionsLogout,
            ("DELETE", ["oauth2", "tokens", _]) => Self::OAuthDeauthorize,
            ("POST", ["reporting", _]) => Self::Reports,
            ("POST", ["guilds"])
            | ("PATCH" | "DELETE", ["guilds", _])
            | ("POST", ["guilds", _, "mfa"]) => Self::ServerAdmin,
            ("POST" | "PATCH", ["guilds", _, "channels"]) => Self::Channels,
            ("POST" | "PATCH", ["guilds", _, "roles"])
            | ("PATCH" | "DELETE", ["guilds", _, "roles", _])
            | ("PUT" | "DELETE", ["guilds", _, "members", _, "roles", _]) => Self::Roles,
            ("PATCH", ["guilds", _, "members", "@me"]) => Self::Profile,
            ("PATCH" | "DELETE", ["guilds", _, "members", _])
            | ("PUT" | "DELETE", ["guilds", _, "bans", _]) => Self::Moderation,
            ("POST", ["guilds", _, "auto-moderation", "rules"])
            | ("PATCH" | "DELETE", ["guilds", _, "auto-moderation", "rules", _]) => Self::AutoMod,
            ("PATCH", ["guilds", _, "member-verification"])
            | ("PUT", ["guilds", _, "onboarding" | "new-member-welcome"]) => Self::CommunityConfig,
            ("PUT", ["guilds", _, "requests", "@me"] | ["guilds", _, "onboarding-responses"]) => {
                Self::Onboarding
            }
            ("POST" | "PATCH" | "DELETE" | "PUT", ["guilds", _, "scheduled-events", ..]) => {
                Self::ScheduledEvents
            }
            (
                "POST" | "PATCH" | "DELETE",
                ["guilds", _, "emojis" | "stickers" | "soundboard-sounds", ..],
            ) => Self::Expressions,
            ("DELETE", ["guilds", _, "integrations", _])
            | ("PATCH" | "DELETE", ["webhooks", _]) => Self::Webhooks,
            ("PATCH", ["guilds", _, "voice-states", _])
            | ("POST", ["stage-instances"])
            | ("PATCH" | "DELETE", ["stage-instances", _]) => Self::Stage,
            _ => Self::Unclassified,
        }
    }
}

/// The group of `POST /channels/{c}/messages` from its body: a poll, stickers,
/// a forward (`message_reference.type == 1`) or TTS make it an extra, uploaded
/// files make it an upload, anything else is a basic message. A missing or
/// unreadable body is `Unclassified`.
fn message_group(body: Option<&[u8]>) -> WriteGroup {
    let Some(value) = body.and_then(|b| serde_json::from_slice::<Value>(b).ok()) else {
        return WriteGroup::Unclassified;
    };
    let filled = |key: &str| match value.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(_) => true,
    };
    if filled("poll")
        || filled("sticker_ids")
        || value["message_reference"]["type"] == 1
        || value["tts"] == true
    {
        WriteGroup::MessageExtras
    } else if filled("attachments") {
        WriteGroup::Uploads
    } else {
        WriteGroup::BasicMessages
    }
}

#[cfg(test)]
mod tests;
