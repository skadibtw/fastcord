//! Server invitations and membership (SPEC §16, issue #90): find invite codes
//! in message text, resolve one (`GET /invites/{code}`), join with it, create
//! one for a channel, list and revoke the invites of a server or channel the
//! account may manage, browse public servers, and leave a server. Every
//! function sends exactly one
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

/// What a created link lasts by default: seven days, unlimited uses.
pub const CREATED_MAX_AGE_SECONDS: u32 = 604_800;
/// Lifetimes a created link may have, in seconds (0 = never) and their names.
pub const CREATE_AGES: [(u32, &str); 7] = [
    (1_800, "30 minutes"),
    (3_600, "1 hour"),
    (21_600, "6 hours"),
    (43_200, "12 hours"),
    (86_400, "1 day"),
    (CREATED_MAX_AGE_SECONDS, "7 days"),
    (0, "never"),
];
/// Use limits a created link may have (0 = no limit).
pub const CREATE_USES: [u8; 7] = [0, 1, 5, 10, 25, 50, 100];
/// Invites kept from one list; the rest are not shown.
pub const MAX_LISTED: usize = 100;
/// Discovered servers asked for per page (Discord accepts at most 48).
pub const DISCOVERY_PAGE: usize = 24;
const MAX_DESCRIPTION_BYTES: usize = 200;

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
    // Every host contains "discord": most messages are ruled out without the
    // lowercase copy below (#156).
    if !content.contains('/')
        || !content
            .as_bytes()
            .windows(7)
            .any(|window| window.eq_ignore_ascii_case(b"discord"))
    {
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

pub(crate) fn cut_at(mut text: String, max: usize) -> String {
    text.truncate(text.floor_char_boundary(max));
    text
}

fn cut(text: String) -> String {
    cut_at(text, MAX_NAME_BYTES)
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

/// When a listed invite stops working.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiry {
    Never,
    /// Unix seconds.
    At(i64),
    /// Discord gave a lifetime but no readable start.
    Unknown,
}

/// One invite of a server or channel list (needs MANAGE_GUILD / MANAGE_CHANNELS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedInvite {
    pub code: String,
    pub channel_name: String,
    pub inviter: String,
    pub uses: u32,
    /// 0 = no limit.
    pub max_uses: u32,
    pub expiry: Expiry,
    pub temporary: bool,
}

#[derive(Deserialize)]
struct WireInviter {
    #[serde(default)]
    username: String,
    global_name: Option<String>,
}

#[derive(Deserialize)]
struct WireManaged {
    code: String,
    channel: Option<WireChannel>,
    inviter: Option<WireInviter>,
    #[serde(default)]
    uses: u32,
    #[serde(default)]
    max_uses: u32,
    #[serde(default)]
    max_age: u32,
    #[serde(default)]
    temporary: bool,
    created_at: Option<String>,
    expires_at: Option<String>,
}

fn unix(text: &str) -> Option<i64> {
    time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
        .ok()
        .map(|at| at.unix_timestamp())
}

/// Reads a `GET /guilds/{id}/invites` or `/channels/{id}/invites` answer: at
/// most [`MAX_LISTED`] invites, entries with an unusable code dropped. `None`
/// for anything that is not a list.
pub fn parse_invites(body: &[u8]) -> Option<Vec<ManagedInvite>> {
    let wire: Vec<WireManaged> = serde_json::from_slice(body).ok()?;
    Some(
        wire.into_iter()
            .filter(|w| (MIN_CODE_BYTES..=MAX_CODE_BYTES).contains(&w.code.len()))
            .filter(|w| w.code.bytes().all(code_byte))
            .take(MAX_LISTED)
            .map(|w| {
                let expiry = match (w.expires_at.as_deref().and_then(unix), w.max_age) {
                    (Some(at), _) => Expiry::At(at),
                    (None, 0) => Expiry::Never,
                    (None, age) => w
                        .created_at
                        .as_deref()
                        .and_then(unix)
                        .map_or(Expiry::Unknown, |at| Expiry::At(at + i64::from(age))),
                };
                let inviter = w.inviter.map_or_else(String::new, |i| {
                    i.global_name
                        .filter(|name| !name.is_empty())
                        .unwrap_or(i.username)
                });
                ManagedInvite {
                    code: w.code,
                    channel_name: w.channel.map(|c| cut(c.name)).unwrap_or_default(),
                    inviter: cut(inviter),
                    uses: w.uses,
                    max_uses: w.max_uses,
                    expiry,
                    temporary: w.temporary,
                }
            })
            .collect(),
    )
}

/// A public server of the discovery list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredGuild {
    pub id: Snowflake,
    pub name: String,
    pub description: String,
    pub icon: Option<String>,
    pub members: Option<u32>,
    pub online: Option<u32>,
}

#[derive(Deserialize)]
struct WireDiscovered {
    id: Snowflake,
    #[serde(default)]
    name: String,
    description: Option<String>,
    icon: Option<String>,
    approximate_member_count: Option<u32>,
    approximate_presence_count: Option<u32>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireDiscovery {
    Wrapped { guilds: Vec<WireDiscovered> },
    Bare(Vec<WireDiscovered>),
}

/// Reads a `GET /discoverable-guilds` answer (an object with `guilds`, or a
/// bare list); at most [`DISCOVERY_PAGE`] servers, texts capped.
pub fn parse_discovery(body: &[u8]) -> Option<Vec<DiscoveredGuild>> {
    let (WireDiscovery::Wrapped { guilds: wire } | WireDiscovery::Bare(wire)) =
        serde_json::from_slice(body).ok()?;
    Some(
        wire.into_iter()
            .take(DISCOVERY_PAGE)
            .map(|g| DiscoveredGuild {
                id: g.id,
                name: cut(g.name),
                description: cut_at(g.description.unwrap_or_default(), MAX_DESCRIPTION_BYTES),
                icon: g.icon.map(cut),
                members: g.approximate_member_count,
                online: g.approximate_presence_count,
            })
            .collect(),
    )
}

/// Where an invite list is read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InviteScope {
    Guild(Snowflake),
    Channel(Snowflake),
}

impl InviteScope {
    fn route(self) -> Result<Route, RestError> {
        match self {
            Self::Guild(id) => Route::new(Method::GET, &format!("/guilds/{id}/invites")),
            Self::Channel(id) => Route::new(Method::GET, &format!("/channels/{id}/invites")),
        }
    }
}

fn discovery_route(offset: usize) -> Result<Route, RestError> {
    Route::new(
        Method::GET,
        &format!("/discoverable-guilds?offset={offset}&limit={DISCOVERY_PAGE}"),
    )
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

fn create_body(max_age: u32, max_uses: u8) -> serde_json::Value {
    serde_json::json!({
        "max_age": max_age,
        "max_uses": max_uses,
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

    /// Creates an invite for a channel with the given lifetime in seconds
    /// (0 = never) and use limit (0 = none), see [`CREATE_AGES`], [`CREATE_USES`].
    pub async fn create_invite(
        &self,
        channel: Snowflake,
        max_age: u32,
        max_uses: u8,
    ) -> Result<String, RestError> {
        let request = RestRequest::new(
            Route::new(Method::POST, &format!("/channels/{channel}/invites"))?,
            Priority::UserWrite,
        )
        .with_error_detail()
        .json(&create_body(max_age, max_uses))?;
        let response = self.execute(request).await?;
        created_code(response.body()).ok_or(RestError::InvalidJson)
    }

    /// The invites of a server (MANAGE_GUILD) or channel (MANAGE_CHANNELS).
    pub async fn list_invites(&self, scope: InviteScope) -> Result<Vec<ManagedInvite>, RestError> {
        let request = RestRequest::new(scope.route()?, Priority::UserRead).with_error_detail();
        let response = self.execute(request).await?;
        parse_invites(response.body()).ok_or(RestError::InvalidJson)
    }

    /// Revokes one invite (`DELETE /invites/{code}`) on an explicit click.
    /// An unknown invite is `ResourceGone` or a 404 `Refused`.
    pub async fn revoke_invite(&self, code: &str) -> Result<(), RestError> {
        let request = RestRequest::new(
            code_route(Method::DELETE, code, false)?,
            Priority::UserWrite,
        )
        .with_error_detail();
        self.execute(request).await.map(drop)
    }

    /// One page of the public server list (unverified user route).
    pub async fn discoverable_guilds(
        &self,
        offset: usize,
    ) -> Result<Vec<DiscoveredGuild>, RestError> {
        let request = RestRequest::new(discovery_route(offset)?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_discovery(response.body()).ok_or(RestError::InvalidJson)
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
            .strip_prefix("https://discord.com/api/v9")
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
        assert_eq!(
            create_body(CREATED_MAX_AGE_SECONDS, 0),
            fixture()["create_request"]
        );
        assert_eq!(create_body(1_800, 5)["max_uses"], 5);
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
    fn an_invite_list_decodes_with_expiry_names_and_bounds() {
        let body = include_bytes!("../../../fixtures/rest/invite-list-synthetic.json");
        let list = parse_invites(body).unwrap();
        // The entry with a path-like code is dropped.
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].code, "abc123");
        assert_eq!(list[0].inviter, "Alice");
        assert_eq!(list[0].channel_name, "general");
        assert_eq!((list[0].uses, list[0].max_uses), (3, 10));
        // expires_at wins (2030-01-01T00:00:00Z).
        assert_eq!(list[0].expiry, Expiry::At(1_893_456_000));
        // No global name: the username. max_age 0 never expires. Temporary.
        assert_eq!(list[1].inviter, "bob");
        assert_eq!(list[1].expiry, Expiry::Never);
        assert!(list[1].temporary);
        // created_at + max_age; no inviter.
        assert_eq!(list[2].expiry, Expiry::At(1_893_456_000 + 3_600));
        assert_eq!(list[2].inviter, "");
        assert!(parse_invites(b"{}").is_none());
        let many: Vec<_> = (0..MAX_LISTED + 20)
            .map(|n| serde_json::json!({"code": format!("c{n:04}")}))
            .collect();
        let many = parse_invites(&serde_json::to_vec(&many).unwrap()).unwrap();
        assert_eq!(many.len(), MAX_LISTED);
        assert_eq!(many[0].expiry, Expiry::Never);
    }

    #[test]
    fn discovery_decodes_wrapped_and_bare_lists_within_bounds() {
        let body = include_bytes!("../../../fixtures/rest/discovery-synthetic.json");
        let page = parse_discovery(body).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, Snowflake(100));
        assert_eq!(page[0].name, "Test Server");
        assert_eq!(page[0].description, "A synthetic server.");
        assert_eq!(page[0].icon.as_deref(), Some("iconhash"));
        assert_eq!((page[0].members, page[0].online), (Some(1200), Some(340)));
        assert_eq!(page[1].description, "");
        assert_eq!(page[1].members, None);
        let bare = parse_discovery(br#"[{"id":"5","name":"x"}]"#).unwrap();
        assert_eq!(bare[0].id, Snowflake(5));
        assert!(parse_discovery(b"{}").is_none());
        let long = serde_json::json!([{
            "id": "1",
            "name": "n".repeat(500),
            "description": "d".repeat(900),
        }]);
        let long = parse_discovery(long.to_string().as_bytes()).unwrap();
        assert_eq!(long[0].name.len(), MAX_NAME_BYTES);
        assert_eq!(long[0].description.len(), MAX_DESCRIPTION_BYTES);
        let many: Vec<_> = (1..=DISCOVERY_PAGE as u64 + 10)
            .map(|id| serde_json::json!({"id": id.to_string()}))
            .collect();
        let many = parse_discovery(&serde_json::to_vec(&many).unwrap()).unwrap();
        assert_eq!(many.len(), DISCOVERY_PAGE);
    }

    #[test]
    fn management_and_discovery_routes_are_fixed_shapes() {
        assert_eq!(
            path(&InviteScope::Guild(Snowflake(7)).route().unwrap()),
            "/guilds/7/invites"
        );
        assert_eq!(
            path(&InviteScope::Channel(Snowflake(8)).route().unwrap()),
            "/channels/8/invites"
        );
        assert_eq!(
            path(&discovery_route(48).unwrap()),
            "/discoverable-guilds?offset=48&limit=24"
        );
        let revoke = code_route(Method::DELETE, "abc123", false).unwrap();
        assert_eq!(path(&revoke), "/invites/abc123");
        assert_eq!(revoke.key().method(), &Method::DELETE);
        const { assert!(DISCOVERY_PAGE <= 48) };
    }

    #[test]
    fn managing_invites_needs_manage_guild_or_manage_channels() {
        use crate::state::navigation::PermissionSummary;
        use fastcord_model::Permissions;
        let none = PermissionSummary::from(Permissions::VIEW_CHANNEL);
        assert!(!none.manage_guild && !none.manage_channel && !none.create_invite);
        let guild = PermissionSummary::from(Permissions::MANAGE_GUILD);
        assert!(guild.manage_guild && !guild.manage_channel);
        let channel = PermissionSummary::from(Permissions::MANAGE_CHANNELS);
        assert!(channel.manage_channel && !channel.manage_guild);
        let admin = PermissionSummary::from(Permissions::ALL);
        assert!(admin.manage_guild && admin.manage_channel && admin.create_invite);
    }

    #[test]
    fn the_owner_cannot_leave_but_a_member_can() {
        assert!(owner_cannot_leave(Some(Snowflake(1)), Snowflake(1)));
        assert!(!owner_cannot_leave(Some(Snowflake(2)), Snowflake(1)));
        assert!(!owner_cannot_leave(None, Snowflake(1)));
    }
}
