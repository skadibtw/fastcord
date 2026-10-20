//! Relationship mutations and user profiles (SPEC §16 milestone 45).
//!
//! Each call is one explicit user action and exactly one request: the
//! transport only reschedules a 429, so a failure reaches the caller, who owns
//! any retry. Nothing here lists, discovers or acts on users in bulk.
//!
//! - accept an incoming request, or send one to a known ID:
//!   `PUT /users/@me/relationships/{id}` with `{}`
//! - send a request by username: `POST /users/@me/relationships`
//! - block: the same PUT with `{"type": 2}`
//! - decline, cancel, remove a friend, unblock: `DELETE` of the same path
//! - ignore / stop ignoring (issue #80; not a block, the relationship type is
//!   untouched): `PUT` / `DELETE` of `.../relationships/{id}/ignore`
//! - profile: `GET /users/{id}/profile` with mutual guild/friend data
//!
//! The wire shapes are from observed web-client traffic and are unverified
//! against a live account (`docs/TESTING.md`).

use fastcord_model::{Snowflake, User};
use reqwest::Method;
use serde::{Deserialize, Serialize};

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Longest username Discord accepts (32) plus a legacy `#0000` suffix.
const MAX_USERNAME_CHARS: usize = 37;
/// Profile bios are at most 190 characters; cap what is kept regardless.
const MAX_BIO_CHARS: usize = 600;
const MAX_PRONOUNS_CHARS: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelationshipAction {
    /// Accept an incoming request, or send one to a known user.
    Befriend,
    Block,
    /// Decline an incoming request, cancel an outgoing one, remove a friend
    /// or unblock: all the same `DELETE`.
    Remove,
    /// Hide the user's messages, notifications and calls without blocking.
    Ignore,
    Unignore,
}

#[derive(Serialize)]
struct UsernameBody<'a> {
    username: &'a str,
    discriminator: Option<u16>,
}

/// The route and JSON body of one action.
fn action_parts(
    action: RelationshipAction,
    user: Snowflake,
) -> Result<(Route, Option<serde_json::Value>), RestError> {
    let path = format!("/users/@me/relationships/{user}");
    Ok(match action {
        RelationshipAction::Befriend => {
            (Route::new(Method::PUT, &path)?, Some(serde_json::json!({})))
        }
        RelationshipAction::Block => (
            Route::new(Method::PUT, &path)?,
            Some(serde_json::json!({"type": 2})),
        ),
        RelationshipAction::Remove => (Route::new(Method::DELETE, &path)?, None),
        RelationshipAction::Ignore => (
            Route::new(Method::PUT, &format!("{path}/ignore"))?,
            Some(serde_json::json!({})),
        ),
        RelationshipAction::Unignore => {
            (Route::new(Method::DELETE, &format!("{path}/ignore"))?, None)
        }
    })
}

fn action_request(action: RelationshipAction, user: Snowflake) -> Result<RestRequest, RestError> {
    let (route, body) = action_parts(action, user)?;
    let request = RestRequest::new(route, Priority::UserWrite);
    match body {
        Some(body) => request.json(&body),
        None => Ok(request),
    }
}

/// Why a username cannot be sent as a friend request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsernameError {
    Empty,
    Invalid,
}

/// Trims and checks a typed username: 2 to 37 characters, no whitespace,
/// control characters or `@`. An optional legacy `#1234` suffix is allowed.
pub fn parse_username(input: &str) -> Result<(&str, Option<u16>), UsernameError> {
    let input = input.trim().trim_start_matches('@');
    if input.is_empty() {
        return Err(UsernameError::Empty);
    }
    if input.chars().count() > MAX_USERNAME_CHARS
        || input
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '@')
    {
        return Err(UsernameError::Invalid);
    }
    let (name, discriminator) = match input.split_once('#') {
        Some((name, tag))
            if tag.len() == 4 && tag.bytes().all(|b| b.is_ascii_digit()) && tag != "0000" =>
        {
            (name, tag.parse().ok())
        }
        Some(_) => return Err(UsernameError::Invalid),
        None => (input, None),
    };
    if name.chars().count() < 2 {
        return Err(UsernameError::Invalid);
    }
    Ok((name, discriminator))
}

/// What a profile popup shows. Everything but the user is optional: an
/// unknown or missing field never fails the whole profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub user: User,
    pub bio: String,
    pub pronouns: String,
    pub mutual_guilds: usize,
    pub mutual_friends: usize,
}

#[derive(Deserialize)]
struct WireProfile {
    user: User,
    #[serde(default)]
    user_profile: Option<serde_json::Value>,
    #[serde(default)]
    mutual_guilds: Option<serde_json::Value>,
    #[serde(default)]
    mutual_friends: Option<serde_json::Value>,
    #[serde(default)]
    mutual_friends_count: Option<serde_json::Value>,
}

fn capped(value: Option<&serde_json::Value>, limit: usize) -> String {
    value
        .and_then(serde_json::Value::as_str)
        .map(|text| text.chars().take(limit).collect())
        .unwrap_or_default()
}

fn count(array: &Option<serde_json::Value>) -> usize {
    array
        .as_ref()
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len)
}

/// Lenient: only a missing `user` is an error.
pub fn parse_profile(body: &[u8]) -> Result<Profile, RestError> {
    let wire: WireProfile = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    let details = wire.user_profile.as_ref();
    let mutual_friends = match wire.mutual_friends_count.as_ref().and_then(|v| v.as_u64()) {
        Some(n) => usize::try_from(n).unwrap_or(usize::MAX),
        None => count(&wire.mutual_friends),
    };
    Ok(Profile {
        bio: capped(details.and_then(|d| d.get("bio")), MAX_BIO_CHARS),
        pronouns: capped(details.and_then(|d| d.get("pronouns")), MAX_PRONOUNS_CHARS),
        mutual_guilds: count(&wire.mutual_guilds),
        mutual_friends,
        user: wire.user,
    })
}

impl<C: Clock> RestClient<C> {
    /// One request for one user the account chose.
    pub async fn relationship_action(
        &self,
        action: RelationshipAction,
        user: Snowflake,
    ) -> Result<(), RestError> {
        self.execute(action_request(action, user)?).await.map(drop)
    }

    /// Sends a friend request to a typed username (one POST).
    pub async fn send_friend_request(&self, username: &str) -> Result<(), RestError> {
        let (username, discriminator) =
            parse_username(username).map_err(|_| RestError::InvalidRoute)?;
        let request = RestRequest::new(
            Route::new(Method::POST, "/users/@me/relationships")?,
            Priority::UserWrite,
        )
        .json(&UsernameBody {
            username,
            discriminator,
        })?;
        self.execute(request).await.map(drop)
    }

    /// Fetches one user's profile because the account asked to see it.
    pub async fn user_profile(&self, user: Snowflake) -> Result<Profile, RestError> {
        let request = RestRequest::new(
            Route::new(
                Method::GET,
                &format!(
                    "/users/{user}/profile?with_mutual_guilds=true&with_mutual_friends_count=true"
                ),
            )?,
            Priority::UserRead,
        );
        parse_profile(self.execute(request).await?.body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_use_one_route_each() {
        let id = Snowflake(42);
        let (route, body) = action_parts(RelationshipAction::Befriend, id).unwrap();
        assert_eq!(route.key().method(), Method::PUT);
        assert_eq!(route.url.path(), "/api/v10/users/@me/relationships/42");
        assert_eq!(body.unwrap(), serde_json::json!({}));
        let (route, body) = action_parts(RelationshipAction::Block, id).unwrap();
        assert_eq!(route.key().method(), Method::PUT);
        assert_eq!(body.unwrap(), serde_json::json!({"type": 2}));
        let (route, body) = action_parts(RelationshipAction::Remove, id).unwrap();
        assert_eq!(route.key().method(), Method::DELETE);
        assert_eq!(route.url.path(), "/api/v10/users/@me/relationships/42");
        assert!(body.is_none());
        // Ignore is its own route and never the block body.
        let (route, body) = action_parts(RelationshipAction::Ignore, id).unwrap();
        assert_eq!(route.key().method(), Method::PUT);
        assert_eq!(
            route.url.path(),
            "/api/v10/users/@me/relationships/42/ignore"
        );
        assert_eq!(body.unwrap(), serde_json::json!({}));
        let (route, body) = action_parts(RelationshipAction::Unignore, id).unwrap();
        assert_eq!(route.key().method(), Method::DELETE);
        assert_eq!(
            route.url.path(),
            "/api/v10/users/@me/relationships/42/ignore"
        );
        assert!(body.is_none());
        for action in [
            RelationshipAction::Befriend,
            RelationshipAction::Block,
            RelationshipAction::Remove,
            RelationshipAction::Ignore,
            RelationshipAction::Unignore,
        ] {
            assert!(action_request(action, id).is_ok());
        }
    }

    #[test]
    fn usernames_are_checked_before_any_request() {
        assert_eq!(parse_username(" @alice "), Ok(("alice", None)));
        assert_eq!(parse_username("alice#0042"), Ok(("alice", Some(42))));
        assert_eq!(parse_username(""), Err(UsernameError::Empty));
        assert_eq!(parse_username("@"), Err(UsernameError::Empty));
        for bad in [
            "a",
            "two words",
            "al@ice",
            "alice#12",
            "alice#0000",
            "alice#x",
        ] {
            assert_eq!(parse_username(bad), Err(UsernameError::Invalid), "{bad}");
        }
        assert_eq!(
            parse_username(&"a".repeat(MAX_USERNAME_CHARS + 1)),
            Err(UsernameError::Invalid)
        );
    }

    #[test]
    fn profile_is_read_leniently_from_the_synthetic_fixture() {
        let profile = parse_profile(include_bytes!(
            "../../../fixtures/rest/user-profile-synthetic.json"
        ))
        .unwrap();
        assert_eq!(profile.user.id, Snowflake(42));
        assert_eq!(profile.bio, "Synthetic bio");
        assert_eq!(profile.mutual_guilds, 2);
        assert_eq!(profile.mutual_friends, 3);
        // Only the user is required.
        let bare = parse_profile(br#"{"user":{"id":"7","username":"bare"}}"#).unwrap();
        assert_eq!(
            (bare.bio.as_str(), bare.mutual_guilds, bare.mutual_friends),
            ("", 0, 0)
        );
        assert!(parse_profile(br#"{"user_profile":{}}"#).is_err());
        let huge = format!(
            r#"{{"user":{{"id":"7","username":"x"}},"user_profile":{{"bio":"{}"}}}}"#,
            "b".repeat(5000)
        );
        assert_eq!(
            parse_profile(huge.as_bytes()).unwrap().bio.chars().count(),
            MAX_BIO_CHARS
        );
    }
}
