//! Server invitations and membership (SPEC §16, issue #90): find invite codes
//! in message text, resolve one (`GET /invites/{code}`), join with it, create
//! one for a channel, and leave a server. Every function sends exactly one
//! request on the user's click (the lookup on a visible card) and never
//! retries. A join or leave that Discord asks to verify surfaces as
//! `RestError::AccountVerificationRequired` and pauses every write; the
//! membership list changes only when `GUILD_CREATE`/`GUILD_DELETE` arrive.
//! Unverified against a live account (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde::Deserialize;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Invite codes taken from one message.
pub const MAX_CODES_PER_MESSAGE: usize = 2;
const MAX_CODE_BYTES: usize = 32;
const MIN_CODE_BYTES: usize = 2;
/// Names copied from an invite are cut to this many bytes.
const MAX_NAME_BYTES: usize = 100;

/// What a created link lasts: seven days, unlimited uses, not temporary.
pub const CREATED_MAX_AGE_SECONDS: u32 = 604_800;

const HOSTS: [&str; 3] = [
    "discord.gg/",
    "discord.com/invite/",
    "discordapp.com/invite/",
];

fn code_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-'
}

/// Invite codes of `content`, in order, without repeats. Only links the web
/// client also treats as invites count: the host must begin a word (or follow
/// `http(s)://` and an optional `www.`), so `notdiscord.gg/x`,
/// `evil.example/discord.gg/x` and `a.discord.gg/x` yield nothing.
pub fn extract_codes(content: &str) -> Vec<String> {
    if !content.contains('/') {
        return Vec::new();
    }
    let lower = content.to_ascii_lowercase();
    let mut found: Vec<(usize, String)> = Vec::new();
    for host in HOSTS {
        let mut from = 0;
        while let Some(at) = lower[from..].find(host) {
            let start = from + at;
            from = start + host.len();
            let before = &lower[..start];
            let before = before.strip_suffix("www.").unwrap_or(before);
            let before = before
                .strip_suffix("https://")
                .or_else(|| before.strip_suffix("http://"))
                .unwrap_or(before);
            let word_start = before.chars().next_back().is_none_or(|c| {
                c.is_whitespace() || matches!(c, '<' | '(' | '[' | '*' | '_' | '~' | '|' | '>')
            });
            if !word_start {
                continue;
            }
            let len = content.as_bytes()[from..]
                .iter()
                .take_while(|b| code_byte(**b))
                .count();
            if !(MIN_CODE_BYTES..=MAX_CODE_BYTES).contains(&len) {
                continue;
            }
            let code = content[from..from + len].to_owned();
            if !found.iter().any(|(_, known)| *known == code) {
                found.push((start, code));
            }
        }
    }
    found.sort_by_key(|(at, _)| *at);
    found
        .into_iter()
        .take(MAX_CODES_PER_MESSAGE)
        .map(|(_, code)| code)
        .collect()
}

#[derive(Deserialize)]
struct WireGuild {
    id: Snowflake,
    #[serde(default)]
    name: String,
    icon: Option<String>,
}

#[derive(Deserialize)]
struct WireChannel {
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct WireInvite {
    code: String,
    guild: Option<WireGuild>,
    channel: Option<WireChannel>,
    approximate_member_count: Option<u32>,
    approximate_presence_count: Option<u32>,
    expires_at: Option<String>,
}

fn cut(mut text: String) -> String {
    if text.len() > MAX_NAME_BYTES {
        let mut end = MAX_NAME_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// A resolved invite: the server it leads to and what Discord counts there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invite {
    pub code: String,
    pub guild: Snowflake,
    pub guild_name: String,
    pub icon: Option<String>,
    pub channel_name: String,
    /// Absent when Discord left the counts out.
    pub members: Option<u32>,
    pub online: Option<u32>,
    /// RFC 3339 expiry; none for a link that never expires.
    pub expires_at: Option<String>,
}

impl Invite {
    /// Reads an invite response. An invite without a server (a group DM or
    /// friend invite) is not one this client can show: `None`.
    pub fn from_json(body: &[u8]) -> Option<Self> {
        let wire: WireInvite = serde_json::from_slice(body).ok()?;
        let guild = wire.guild?;
        Some(Self {
            code: cut(wire.code),
            guild: guild.id,
            guild_name: cut(guild.name),
            icon: guild.icon.map(cut),
            channel_name: wire.channel.map(|c| cut(c.name)).unwrap_or_default(),
            members: wire.approximate_member_count,
            online: wire.approximate_presence_count,
            expires_at: wire.expires_at.map(cut),
        })
    }
}

/// The code of a `POST /channels/{id}/invites` response.
pub fn created_code(body: &[u8]) -> Option<String> {
    #[derive(Deserialize)]
    struct Created {
        code: String,
    }
    let code = serde_json::from_slice::<Created>(body).ok()?.code;
    (code.len() <= MAX_CODE_BYTES && code.bytes().all(code_byte)).then_some(code)
}

/// Whether an account may not leave its server.
pub fn owner_cannot_leave(owner: Option<Snowflake>, me: Snowflake) -> bool {
    owner == Some(me)
}

fn code_route(method: Method, code: &str, counts: bool) -> Result<Route, RestError> {
    if !(MIN_CODE_BYTES..=MAX_CODE_BYTES).contains(&code.len()) || !code.bytes().all(code_byte) {
        return Err(RestError::InvalidRoute);
    }
    let query = if counts { "?with_counts=true" } else { "" };
    Route::new(method, &format!("/invites/{code}{query}"))
}

fn create_body() -> serde_json::Value {
    serde_json::json!({
        "max_age": CREATED_MAX_AGE_SECONDS,
        "max_uses": 0,
        "temporary": false,
    })
}

fn leave_body() -> serde_json::Value {
    serde_json::json!({ "lurking": false })
}

impl<C: Clock> RestClient<C> {
    /// Resolves a code for its card. `Err(ResourceGone)` is an expired,
    /// revoked or unknown invite.
    pub async fn lookup_invite(&self, code: &str) -> Result<Option<Invite>, RestError> {
        let request = RestRequest::new(
            code_route(Method::GET, code, true)?,
            Priority::SpeculativeRead,
        );
        let response = self.execute(request).await?;
        Ok(Invite::from_json(response.body()))
    }

    /// Joins the server of an invite (`POST /invites/{code}`) on an explicit
    /// click. A CAPTCHA or verification answer pauses writes
    /// (`AccountVerificationRequired`); a refusal keeps Discord's reason.
    pub async fn join_invite(&self, code: &str) -> Result<Option<Invite>, RestError> {
        let request = RestRequest::new(code_route(Method::POST, code, false)?, Priority::UserWrite)
            .with_error_detail()
            .json(&serde_json::json!({}))?;
        let response = self.execute(request).await?;
        Ok(Invite::from_json(response.body()))
    }

    /// Creates a seven-day, unlimited-use invite for a channel.
    pub async fn create_invite(&self, channel: Snowflake) -> Result<String, RestError> {
        let request = RestRequest::new(
            Route::new(Method::POST, &format!("/channels/{channel}/invites"))?,
            Priority::UserWrite,
        )
        .with_error_detail()
        .json(&create_body())?;
        let response = self.execute(request).await?;
        created_code(response.body()).ok_or(RestError::InvalidJson)
    }

    /// Leaves a server (`DELETE /users/@me/guilds/{id}`).
    pub async fn leave_guild(&self, guild: Snowflake) -> Result<(), RestError> {
        let request = RestRequest::new(
            Route::new(Method::DELETE, &format!("/users/@me/guilds/{guild}"))?,
            Priority::UserWrite,
        )
        .with_error_detail()
        .json(&leave_body())?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(route: &Route) -> String {
        route
            .url
            .as_str()
            .strip_prefix("https://discord.com/api/v10")
            .unwrap()
            .to_owned()
    }

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/rest/invites-synthetic.json"
        ))
        .unwrap()
    }

    #[test]
    fn codes_come_from_every_link_form_and_nothing_else() {
        assert_eq!(
            extract_codes("join https://discord.gg/AbC-123 now"),
            ["AbC-123"]
        );
        assert_eq!(extract_codes("discord.gg/hello"), ["hello"]);
        assert_eq!(
            extract_codes("HTTP://DISCORD.COM/invite/Xy12?x=1"),
            ["Xy12"]
        );
        assert_eq!(
            extract_codes("<https://discordapp.com/invite/zz99>"),
            ["zz99"]
        );
        assert_eq!(extract_codes("[go](https://www.discord.gg/abcd)"), ["abcd"]);
        assert_eq!(extract_codes("**discord.gg/bold**"), ["bold"]);
        assert_eq!(
            extract_codes("discord.gg/ok but discord.com/channels/1/2"),
            ["ok"]
        );
        // Repeats collapse and the count is bounded, in reading order.
        assert_eq!(
            extract_codes("discord.gg/aa discord.com/invite/bb discord.gg/aa discord.gg/cc"),
            ["aa", "bb"]
        );
        let long = format!("discord.gg/{}", "a".repeat(33));
        for no in [
            "notdiscord.gg/abc",
            "a.discord.gg/abc",
            "https://evil.example/discord.gg/abc",
            "https://evil.example/?next=discord.gg/abc",
            "ftp://discord.gg/abc",
            "discord.gg/",
            "discord.gg/a",
            "discord.com/invitefoo/abc",
            "discord.com/channels/1/2/3",
            long.as_str(),
            "no invite here",
        ] {
            assert!(extract_codes(no).is_empty(), "{no}");
        }
    }

    #[test]
    fn an_invite_decodes_with_and_without_counts() {
        let full = Invite::from_json(fixture()["full"].to_string().as_bytes()).unwrap();
        assert_eq!(full.code, "abc123");
        assert_eq!(full.guild, Snowflake(100));
        assert_eq!(full.guild_name, "Test Server");
        assert_eq!(full.icon.as_deref(), Some("iconhash"));
        assert_eq!(full.channel_name, "general");
        assert_eq!((full.members, full.online), (Some(1200), Some(340)));
        assert_eq!(
            full.expires_at.as_deref(),
            Some("2030-01-01T00:00:00+00:00")
        );
        let bare = Invite::from_json(fixture()["no_counts"].to_string().as_bytes()).unwrap();
        assert_eq!(
            (bare.members, bare.online, bare.expires_at),
            (None, None, None)
        );
        assert_eq!(bare.icon, None);
        // A group DM invite has no server; garbage is not an invite.
        assert!(Invite::from_json(fixture()["no_guild"].to_string().as_bytes()).is_none());
        assert!(Invite::from_json(b"[]").is_none());
    }

    #[test]
    fn an_expired_invite_is_the_unknown_invite_refusal() {
        let body = fixture()["expired"].to_string();
        let refusal = crate::Refusal::from_body(reqwest::StatusCode::NOT_FOUND, body.as_bytes());
        assert_eq!(refusal.code, Some(10006));
        assert_eq!(refusal.status.as_u16(), 404);
    }

    #[test]
    fn request_shapes_are_one_route_and_a_minimal_body() {
        let lookup = code_route(Method::GET, "abc123", true).unwrap();
        assert_eq!(path(&lookup), "/invites/abc123?with_counts=true");
        assert_eq!(lookup.key().method(), &Method::GET);
        let join = code_route(Method::POST, "abc123", false).unwrap();
        assert_eq!(path(&join), "/invites/abc123");
        assert_eq!(join.key().method(), &Method::POST);
        assert_eq!(create_body(), fixture()["create_request"]);
        assert_eq!(leave_body(), fixture()["leave_request"]);
        assert_eq!(
            created_code(fixture()["create_response"].to_string().as_bytes()).as_deref(),
            Some("newcode9")
        );
        assert_eq!(created_code(br#"{"code":"a/b"}"#), None);
        // A code that could change the route is refused before anything is sent.
        let long = "a".repeat(33);
        for bad in ["", "a", "../x", "a/b", "ab?x", "ab cd", long.as_str()] {
            assert!(code_route(Method::GET, bad, true).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_owner_cannot_leave_but_a_member_can() {
        assert!(owner_cannot_leave(Some(Snowflake(1)), Snowflake(1)));
        assert!(!owner_cannot_leave(Some(Snowflake(2)), Snowflake(1)));
        assert!(!owner_cannot_leave(None, Snowflake(1)));
    }
}
