//! Manual moderation beyond the member menu (issue #95): the ban list and
//! Unban, bulk message deletion and the read-only audit log. Each call is one
//! explicit user action and one request, sent once and never retried. Nothing
//! here scrapes members or watches content: lists are paged and bounded and are
//! read only when the user opens them. Reasons, names and change values are
//! private: no `Debug` output prints them. Wire shapes follow Discord's
//! documentation and are unverified against a live user account
//! (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde::Deserialize;
use serde_json::Value;
use std::fmt;

use crate::invite::cut_at;
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Bans per page.
pub const BANS_PAGE: usize = 50;
/// Audit entries per page, the most the web client asks for.
pub const AUDIT_PAGE: usize = 50;
/// Messages in one bulk deletion (Discord accepts 2 to 100).
pub const BULK_MAX: usize = 100;
pub const BULK_MIN: usize = 2;
/// Bulk deletion refuses messages 14 days old; one minute less is allowed so
/// that a message on the edge is never sent.
pub const BULK_AGE_MS: u64 = 14 * 86_400_000 - 60_000;
const MAX_NAME_BYTES: usize = 128;
const MAX_REASON_BYTES: usize = 512;
const MAX_VALUE_BYTES: usize = 40;
const MAX_SUMMARY_BYTES: usize = 220;
const SUMMARISED_CHANGES: usize = 3;
const MAX_AUDIT_USERS: usize = 200;

/// Typed audit-log reason text cut to the 512 bytes Discord keeps.
pub fn cut_reason(text: String) -> String {
    cut_at(text, MAX_REASON_BYTES)
}

fn write(method: Method, path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(Route::new(method, path)?, Priority::UserWrite).with_error_detail())
}

/// A banned user, as the list shows them.
#[derive(Clone, PartialEq, Eq)]
pub struct BanEntry {
    pub user: Snowflake,
    pub name: String,
    pub reason: Option<String>,
}

impl fmt::Debug for BanEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BanEntry")
            .field("user", &self.user)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BansPage {
    pub bans: Vec<BanEntry>,
    /// The page was full, so more may follow after its last user.
    pub has_more: bool,
}

#[derive(Deserialize)]
struct WireUser {
    id: Snowflake,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    global_name: Option<String>,
}

impl WireUser {
    fn name(self) -> (Snowflake, String) {
        let name = self
            .global_name
            .filter(|n| !n.is_empty())
            .or(self.username)
            .unwrap_or_default();
        (self.id, cut_at(name, MAX_NAME_BYTES))
    }
}

#[derive(Deserialize)]
struct WireBan {
    user: WireUser,
    #[serde(default)]
    reason: Option<String>,
}

/// Decodes a page of bans, keeping at most `limit`.
pub fn parse_bans(body: &[u8], limit: usize) -> Result<BansPage, RestError> {
    let wire: Vec<WireBan> = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    let has_more = wire.len() >= limit;
    let bans = wire
        .into_iter()
        .take(limit)
        .map(|ban| {
            let (user, name) = ban.user.name();
            BanEntry {
                user,
                name,
                reason: ban
                    .reason
                    .filter(|r| !r.is_empty())
                    .map(|r| cut_at(r, MAX_REASON_BYTES)),
            }
        })
        .collect();
    Ok(BansPage { bans, has_more })
}

fn bans_path(guild: Snowflake, after: Option<Snowflake>) -> String {
    match after {
        Some(after) => format!("/guilds/{guild}/bans?limit={BANS_PAGE}&after={after}"),
        None => format!("/guilds/{guild}/bans?limit={BANS_PAGE}"),
    }
}

/// Why a bulk deletion was not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkError {
    TooFew,
    TooMany,
    /// A message is 14 days old or older (or from the future).
    TooOld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BulkRequestError {
    Invalid(BulkError),
    Rest(RestError),
}

impl From<RestError> for BulkRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

/// Whether a message is young enough for bulk deletion at `now_ms` (Unix ms).
pub fn bulk_deletable(message: Snowflake, now_ms: u64) -> bool {
    let at = message.timestamp_ms();
    at <= now_ms && now_ms - at < BULK_AGE_MS
}

/// The request body for a bulk deletion, checked against the limits.
pub fn bulk_body(messages: &[Snowflake], now_ms: u64) -> Result<Value, BulkError> {
    if messages.len() < BULK_MIN {
        return Err(BulkError::TooFew);
    }
    if messages.len() > BULK_MAX {
        return Err(BulkError::TooMany);
    }
    if !messages.iter().all(|m| bulk_deletable(*m, now_ms)) {
        return Err(BulkError::TooOld);
    }
    let ids: Vec<String> = messages.iter().map(ToString::to_string).collect();
    Ok(serde_json::json!({ "messages": ids }))
}

/// Audit-log action types the filter offers: the moderation ones.
pub const AUDIT_FILTERS: [(u32, &str); 12] = [
    (20, "Member kick"),
    (22, "Member ban"),
    (23, "Member unban"),
    (24, "Member update"),
    (25, "Member role update"),
    (26, "Member move"),
    (27, "Member disconnect"),
    (72, "Message delete"),
    (73, "Message bulk delete"),
    (11, "Channel update"),
    (31, "Role update"),
    (111, "Thread update"),
];

/// A readable name for an audit-log action type; unknown ones show their number.
pub fn action_name(action: u32) -> String {
    match action {
        1 => "Server updated",
        10 => "Channel created",
        11 => "Channel updated",
        12 => "Channel deleted",
        13 => "Channel overwrite created",
        14 => "Channel overwrite updated",
        15 => "Channel overwrite deleted",
        20 => "Member kicked",
        21 => "Members pruned",
        22 => "Member banned",
        23 => "Member unbanned",
        24 => "Member updated",
        25 => "Member roles updated",
        26 => "Member moved",
        27 => "Member disconnected",
        28 => "Bot added",
        30 => "Role created",
        31 => "Role updated",
        32 => "Role deleted",
        40 => "Invite created",
        41 => "Invite updated",
        42 => "Invite deleted",
        50 => "Webhook created",
        51 => "Webhook updated",
        52 => "Webhook deleted",
        60 => "Emoji created",
        61 => "Emoji updated",
        62 => "Emoji deleted",
        72 => "Message deleted",
        73 => "Messages bulk deleted",
        74 => "Message pinned",
        75 => "Message unpinned",
        80 => "Integration created",
        81 => "Integration updated",
        82 => "Integration deleted",
        83 => "Stage started",
        84 => "Stage updated",
        85 => "Stage ended",
        90 => "Sticker created",
        91 => "Sticker updated",
        92 => "Sticker deleted",
        100 => "Event created",
        101 => "Event updated",
        102 => "Event deleted",
        110 => "Thread created",
        111 => "Thread updated",
        112 => "Thread deleted",
        140 => "Auto-moderation rule created",
        141 => "Auto-moderation rule updated",
        142 => "Auto-moderation rule deleted",
        143 => "Message blocked by auto-moderation",
        144 => "Message flagged by auto-moderation",
        145 => "Member timed out by auto-moderation",
        other => return format!("Action {other}"),
    }
    .to_owned()
}

/// One audit-log entry reduced to what the panel shows. Names come from the
/// answer's own `users` list; targets that are not users show their ID.
#[derive(Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: Snowflake,
    pub actor: Option<Snowflake>,
    pub actor_name: Option<String>,
    pub target: Option<Snowflake>,
    pub target_name: Option<String>,
    pub action: u32,
    pub reason: Option<String>,
    /// The changes in one line, empty when there are none.
    pub summary: String,
}

impl fmt::Debug for AuditEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuditEntry")
            .field("id", &self.id)
            .field("action", &self.action)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditPage {
    pub entries: Vec<AuditEntry>,
    pub has_more: bool,
}

/// Filters and the cursor of one audit-log request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AuditQuery {
    pub action_type: Option<u32>,
    pub user: Option<Snowflake>,
    /// Entries older than this entry.
    pub before: Option<Snowflake>,
}

fn audit_path(guild: Snowflake, query: AuditQuery) -> String {
    let mut path = format!("/guilds/{guild}/audit-logs?limit={AUDIT_PAGE}");
    if let Some(before) = query.before {
        path.push_str(&format!("&before={before}"));
    }
    if let Some(action) = query.action_type {
        path.push_str(&format!("&action_type={action}"));
    }
    if let Some(user) = query.user {
        path.push_str(&format!("&user_id={user}"));
    }
    path
}

#[derive(Deserialize)]
struct WireChange {
    key: String,
    #[serde(default)]
    old_value: Option<Value>,
    #[serde(default)]
    new_value: Option<Value>,
}

#[derive(Deserialize)]
struct WireEntry {
    id: Snowflake,
    #[serde(default)]
    user_id: Option<Snowflake>,
    #[serde(default)]
    target_id: Option<String>,
    action_type: u32,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    changes: Vec<WireChange>,
}

#[derive(Deserialize)]
struct WireLog {
    #[serde(default)]
    audit_log_entries: Vec<WireEntry>,
    #[serde(default)]
    users: Vec<WireUser>,
}

fn show(value: &Value) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    cut_at(text, MAX_VALUE_BYTES)
}

/// `key: old -> new` for the first few changes, then how many more.
fn summarise(changes: &[WireChange]) -> String {
    let mut parts: Vec<String> = changes
        .iter()
        .take(SUMMARISED_CHANGES)
        .map(|change| {
            let old = change.old_value.as_ref().filter(|v| !v.is_null());
            let new = change.new_value.as_ref().filter(|v| !v.is_null());
            match (old, new) {
                (Some(old), Some(new)) => {
                    format!("{}: {} \u{2192} {}", change.key, show(old), show(new))
                }
                (None, Some(new)) => format!("{}: {}", change.key, show(new)),
                (Some(old), None) => format!("{} removed ({})", change.key, show(old)),
                (None, None) => change.key.clone(),
            }
        })
        .collect();
    if changes.len() > SUMMARISED_CHANGES {
        parts.push(format!("+{} more", changes.len() - SUMMARISED_CHANGES));
    }
    cut_at(parts.join("; "), MAX_SUMMARY_BYTES)
}

/// Decodes one page of the audit log, keeping at most [`AUDIT_PAGE`] entries
/// and the names of at most 200 users.
pub fn parse_audit(body: &[u8]) -> Result<AuditPage, RestError> {
    let wire: WireLog = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    let has_more = wire.audit_log_entries.len() >= AUDIT_PAGE;
    let users: Vec<(Snowflake, String)> = wire
        .users
        .into_iter()
        .take(MAX_AUDIT_USERS)
        .map(WireUser::name)
        .collect();
    let name = |id: Option<Snowflake>| {
        id.and_then(|id| users.iter().find(|(user, _)| *user == id))
            .map(|(_, name)| name.clone())
            .filter(|name| !name.is_empty())
    };
    let entries = wire
        .audit_log_entries
        .into_iter()
        .take(AUDIT_PAGE)
        .map(|entry| {
            let target = entry
                .target_id
                .as_deref()
                .and_then(|id| id.parse::<Snowflake>().ok());
            AuditEntry {
                id: entry.id,
                actor: entry.user_id,
                actor_name: name(entry.user_id),
                target,
                target_name: name(target),
                action: entry.action_type,
                reason: entry
                    .reason
                    .filter(|r| !r.is_empty())
                    .map(|r| cut_at(r, MAX_REASON_BYTES)),
                summary: summarise(&entry.changes),
            }
        })
        .collect();
    Ok(AuditPage { entries, has_more })
}

impl<C: Clock> RestClient<C> {
    /// One page of the ban list (`GET /guilds/{g}/bans`), users after `after`.
    pub async fn list_bans(
        &self,
        guild: Snowflake,
        after: Option<Snowflake>,
    ) -> Result<BansPage, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, &bans_path(guild, after))?,
            Priority::UserRead,
        )
        .with_error_detail();
        let response = self.execute(request).await?;
        parse_bans(response.body(), BANS_PAGE)
    }

    /// Lifts one ban (`DELETE /guilds/{g}/bans/{u}`).
    pub async fn unban(&self, guild: Snowflake, user: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/guilds/{guild}/bans/{user}"))?;
        self.execute(request).await.map(drop)
    }

    /// Deletes 2 to 100 messages younger than 14 days at once
    /// (`POST /channels/{c}/messages/bulk-delete`). `now_ms` is the Unix time
    /// the age is judged at. Discord refuses the whole request if any message
    /// does not qualify.
    pub async fn bulk_delete(
        &self,
        channel: Snowflake,
        messages: &[Snowflake],
        now_ms: u64,
    ) -> Result<(), BulkRequestError> {
        let body = bulk_body(messages, now_ms).map_err(BulkRequestError::Invalid)?;
        let request = write(
            Method::POST,
            &format!("/channels/{channel}/messages/bulk-delete"),
        )?
        .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// One page of the audit log (`GET /guilds/{g}/audit-logs`).
    pub async fn audit_log(
        &self,
        guild: Snowflake,
        query: AuditQuery,
    ) -> Result<AuditPage, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, &audit_path(guild, query))?,
            Priority::UserRead,
        )
        .with_error_detail();
        let response = self.execute(request).await?;
        parse_audit(response.body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_model::DISCORD_EPOCH_MS;

    const BANS: &str = include_str!("../../../fixtures/rest/guild-bans.json");
    const AUDIT: &str = include_str!("../../../fixtures/rest/audit-log.json");

    /// A snowflake created `ms` after the Unix time `at`.
    fn made(at_ms: u64) -> Snowflake {
        Snowflake((at_ms - DISCORD_EPOCH_MS) << 22)
    }

    #[test]
    fn bans_are_parsed_bounded_and_page_by_user_id() {
        let page = parse_bans(BANS.as_bytes(), 50).unwrap();
        assert_eq!(page.bans.len(), 2);
        assert_eq!(page.bans[0].user, Snowflake(1_000_000_000_000_000_201));
        assert_eq!(page.bans[0].name, "Ada");
        assert_eq!(page.bans[1].name, "grace");
        assert_eq!(page.bans[1].reason, None);
        assert!(!page.has_more);
        // A full page may have more; nothing past the limit is kept.
        let page = parse_bans(BANS.as_bytes(), 1).unwrap();
        assert_eq!((page.bans.len(), page.has_more), (1, true));
        assert_eq!(parse_bans(b"{}", 50), Err(RestError::InvalidJson));
        assert_eq!(bans_path(Snowflake(1), None), "/guilds/1/bans?limit=50");
        assert_eq!(
            bans_path(Snowflake(1), Some(Snowflake(9))),
            "/guilds/1/bans?limit=50&after=9"
        );
    }

    #[test]
    fn a_long_reason_is_cut_and_not_printed() {
        let long = "r".repeat(2_000);
        let body = format!(r#"[{{"reason":"{long}","user":{{"id":"7","username":"x"}}}}]"#);
        let page = parse_bans(body.as_bytes(), 50).unwrap();
        assert_eq!(
            page.bans[0].reason.as_ref().unwrap().len(),
            MAX_REASON_BYTES
        );
        assert!(!format!("{page:?}").contains("rrrr"));
    }

    #[test]
    fn bulk_deletion_checks_count_and_age_against_an_injected_clock() {
        let now = DISCORD_EPOCH_MS + 40 * 86_400_000;
        let day = 86_400_000;
        let young = made(now - day);
        let old = made(now - 14 * day);
        assert!(bulk_deletable(young, now));
        assert!(!bulk_deletable(old, now));
        // The cutoff is exact: just inside passes, the 14 day mark does not.
        assert!(bulk_deletable(made(now - BULK_AGE_MS + 1), now));
        assert!(!bulk_deletable(made(now - BULK_AGE_MS), now));
        // A message from the future is not accepted either.
        assert!(!bulk_deletable(made(now + day), now));
        assert_eq!(bulk_body(&[young], now), Err(BulkError::TooFew));
        assert_eq!(bulk_body(&[young, old], now), Err(BulkError::TooOld));
        let many: Vec<Snowflake> = (0..=BULK_MAX as u64).map(|n| made(now - day + n)).collect();
        assert_eq!(bulk_body(&many, now), Err(BulkError::TooMany));
        let body = bulk_body(&many[..BULK_MAX], now).unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), BULK_MAX);
        assert_eq!(
            bulk_body(&many[..2], now).unwrap()["messages"][0],
            many[0].to_string()
        );
    }

    #[test]
    fn audit_entries_name_actor_target_reason_and_changes() {
        let page = parse_audit(AUDIT.as_bytes()).unwrap();
        assert_eq!(page.entries.len(), 3);
        assert!(!page.has_more);
        let ban = &page.entries[0];
        assert_eq!(ban.action, 22);
        assert_eq!(ban.actor_name.as_deref(), Some("Ada"));
        assert_eq!(ban.target_name.as_deref(), Some("grace"));
        assert_eq!(ban.reason.as_deref(), Some("spam"));
        assert_eq!(ban.summary, "");
        let update = &page.entries[1];
        assert_eq!(
            update.summary,
            "nick: old \u{2192} new; mute: true; deaf: true; +1 more"
        );
        // A target that is not a user, and an unknown action type.
        let other = &page.entries[2];
        assert_eq!(other.target, Some(Snowflake(1_000_000_000_000_000_500)));
        assert_eq!(other.target_name, None);
        assert_eq!(action_name(other.action), "Action 9999");
        assert_eq!(action_name(22), "Member banned");
        assert!(!format!("{page:?}").contains("spam"));
    }

    #[test]
    fn audit_requests_carry_filters_and_the_cursor() {
        let g = Snowflake(5);
        assert_eq!(
            audit_path(g, AuditQuery::default()),
            "/guilds/5/audit-logs?limit=50"
        );
        let query = AuditQuery {
            action_type: Some(22),
            user: Some(Snowflake(8)),
            before: Some(Snowflake(9)),
        };
        assert_eq!(
            audit_path(g, query),
            "/guilds/5/audit-logs?limit=50&before=9&action_type=22&user_id=8"
        );
        assert!(parse_audit(b"3").is_err());
        assert!(
            parse_audit(br#"{"audit_log_entries":[]}"#)
                .unwrap()
                .entries
                .is_empty()
        );
    }

    #[test]
    fn a_full_audit_page_is_cut_to_the_limit() {
        let entries: Vec<String> = (0..60)
            .map(|n| format!(r#"{{"id":"{}","action_type":22}}"#, n + 1))
            .collect();
        let body = format!(r#"{{"audit_log_entries":[{}]}}"#, entries.join(","));
        let page = parse_audit(body.as_bytes()).unwrap();
        assert_eq!((page.entries.len(), page.has_more), (AUDIT_PAGE, true));
    }
}
