//! Free account identity edits (SPEC §16, issue #79): display name, static
//! avatar, bio and pronouns, the account's own server nickname, and which
//! connections its profile shows. Each function sends exactly one request on
//! the user's click and never retries; a failure leaves the typed values with
//! the caller. Username changes need the account password (and 2FA when it is
//! on) and are never automated. Paid profile cosmetics (banner, animated
//! avatar, theme colors, effects, per-server profiles) are not offered.
//! Wire shapes are from community documentation of the official client and
//! are unverified against a live account (`docs/TESTING.md`).

use fastcord_model::{Snowflake, User};
use reqwest::Method;
use serde::Deserialize;

use crate::image_data::{ImageError, data_uri};
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

pub const MAX_DISPLAY_NAME_CHARS: usize = 32;
pub const MAX_BIO_CHARS: usize = 190;
pub const MAX_PRONOUNS_CHARS: usize = 40;
pub const MAX_NICK_CHARS: usize = 32;
/// Connections kept from the answer.
pub const MAX_CONNECTIONS: usize = 32;
const MAX_CONNECTION_NAME_CHARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileError {
    TooLong,
    /// Not a PNG or JPEG (animated avatars need Nitro), or empty.
    UnsupportedImage,
    ImageTooLarge,
    /// A connection whose identity cannot be put in a route safely.
    UneditableConnection,
}

/// A validated static avatar, already in the `data:` form the body carries.
#[derive(Clone, PartialEq, Eq)]
pub struct AvatarImage(String);

impl std::fmt::Debug for AvatarImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AvatarImage({} bytes)", self.0.len())
    }
}

impl AvatarImage {
    /// PNG or JPEG of at most 512 KiB, judged by signature. GIF is refused
    /// because an animated avatar is a paid feature.
    pub fn new(bytes: &[u8]) -> Result<Self, ProfileError> {
        data_uri(bytes, false)
            .map(Self)
            .map_err(|error| match error {
                ImageError::Unsupported => ProfileError::UnsupportedImage,
                ImageError::TooLarge => ProfileError::ImageTooLarge,
            })
    }
}

/// One change to the account itself (`PATCH /users/@me`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AccountEdit {
    /// An empty name removes it, so the username is shown.
    DisplayName(String),
    /// `None` removes the avatar.
    Avatar(Option<AvatarImage>),
}

/// Bio and pronouns; only the `Some` fields are sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileText {
    pub bio: Option<String>,
    pub pronouns: Option<String>,
}

fn within(text: &str, limit: usize) -> Result<&str, ProfileError> {
    let text = text.trim();
    if text.chars().count() > limit {
        return Err(ProfileError::TooLong);
    }
    Ok(text)
}

fn account_body(edit: &AccountEdit) -> Result<serde_json::Value, ProfileError> {
    Ok(match edit {
        AccountEdit::DisplayName(name) => {
            let name = within(name, MAX_DISPLAY_NAME_CHARS)?;
            serde_json::json!({ "global_name": (!name.is_empty()).then_some(name) })
        }
        AccountEdit::Avatar(avatar) => {
            serde_json::json!({ "avatar": avatar.as_ref().map(|a| &a.0) })
        }
    })
}

fn profile_body(text: &ProfileText) -> Result<serde_json::Value, ProfileError> {
    let mut body = serde_json::Map::new();
    if let Some(bio) = &text.bio {
        body.insert("bio".into(), within(bio, MAX_BIO_CHARS)?.into());
    }
    if let Some(pronouns) = &text.pronouns {
        body.insert(
            "pronouns".into(),
            within(pronouns, MAX_PRONOUNS_CHARS)?.into(),
        );
    }
    Ok(body.into())
}

fn nick_body(nick: &str) -> Result<serde_json::Value, ProfileError> {
    let nick = within(nick, MAX_NICK_CHARS)?;
    Ok(serde_json::json!({ "nick": (!nick.is_empty()).then_some(nick) }))
}

/// One linked account as the profile can show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub kind: String,
    pub id: String,
    pub name: String,
    pub visible: bool,
    /// "Show activity"; `None` when Discord did not send it.
    pub show_activity: Option<bool>,
    /// False when `kind` or `id` could not be placed in a route safely.
    pub editable: bool,
}

fn route_safe(part: &str) -> bool {
    !part.is_empty()
        && part.len() <= 64
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

#[derive(Deserialize)]
struct WireConnection {
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    visibility: serde_json::Value,
    #[serde(default)]
    show_activity: Option<bool>,
}

/// Lenient: unknown fields are ignored, at most [`MAX_CONNECTIONS`] are kept,
/// and `visibility` is `1`/`true` (visible) or `0`/`false`.
pub fn parse_connections(body: &[u8]) -> Result<Vec<Connection>, RestError> {
    let wire: Vec<WireConnection> =
        serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    Ok(wire
        .into_iter()
        .take(MAX_CONNECTIONS)
        .map(|c| Connection {
            editable: route_safe(&c.kind) && route_safe(&c.id),
            visible: matches!(&c.visibility, serde_json::Value::Bool(true))
                || c.visibility.as_u64() == Some(1),
            show_activity: c.show_activity,
            name: c.name.chars().take(MAX_CONNECTION_NAME_CHARS).collect(),
            kind: c.kind,
            id: c.id,
        })
        .collect())
}

fn connection_route(connection: &Connection) -> Result<Route, ProfileRequestError> {
    if !connection.editable {
        return Err(ProfileRequestError::Invalid(
            ProfileError::UneditableConnection,
        ));
    }
    Ok(Route::new(
        Method::PATCH,
        &format!(
            "/users/@me/connections/{}/{}",
            connection.kind, connection.id
        ),
    )?)
}

/// A failed edit: invalid input (nothing sent) or Discord's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProfileRequestError {
    Invalid(ProfileError),
    Rest(RestError),
}

impl From<RestError> for ProfileRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

impl<C: Clock> RestClient<C> {
    /// Sets or clears the display name or avatar and returns the account as
    /// Discord now has it (`PATCH /users/@me`).
    pub async fn edit_account(&self, edit: &AccountEdit) -> Result<User, ProfileRequestError> {
        let body = account_body(edit).map_err(ProfileRequestError::Invalid)?;
        let request = RestRequest::new(
            Route::new(Method::PATCH, "/users/@me")?,
            Priority::UserWrite,
        )
        .json(&body)?;
        Ok(self.execute(request).await?.json::<User>()?)
    }

    /// Sets bio and/or pronouns (`PATCH /users/@me/profile`).
    pub async fn edit_profile_text(&self, text: &ProfileText) -> Result<(), ProfileRequestError> {
        let body = profile_body(text).map_err(ProfileRequestError::Invalid)?;
        let request = RestRequest::new(
            Route::new(Method::PATCH, "/users/@me/profile")?,
            Priority::UserWrite,
        )
        .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Sets or clears the account's own nickname in one server
    /// (`PATCH /guilds/{id}/members/@me`). An empty nickname clears it.
    pub async fn edit_own_nick(
        &self,
        guild: Snowflake,
        nick: &str,
    ) -> Result<(), ProfileRequestError> {
        let body = nick_body(nick).map_err(ProfileRequestError::Invalid)?;
        let request = RestRequest::new(
            Route::new(Method::PATCH, &format!("/guilds/{guild}/members/@me"))?,
            Priority::UserWrite,
        )
        .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// The account's linked accounts (`GET /users/@me/connections`), asked for
    /// only when the user opens the section.
    pub async fn connections(&self) -> Result<Vec<Connection>, RestError> {
        let request = RestRequest::new(
            Route::new(Method::GET, "/users/@me/connections")?,
            Priority::UserRead,
        );
        parse_connections(self.execute(request).await?.body())
    }

    /// Shows or hides one connection on the profile.
    pub async fn set_connection_visibility(
        &self,
        connection: &Connection,
        visible: bool,
    ) -> Result<(), ProfileRequestError> {
        let request = RestRequest::new(connection_route(connection)?, Priority::UserWrite)
            .json(&serde_json::json!({ "visibility": visible }))?;
        self.execute(request).await?;
        Ok(())
    }

    /// Turns "Show activity" of one connection on or off (issue #101).
    pub async fn set_connection_show_activity(
        &self,
        connection: &Connection,
        show: bool,
    ) -> Result<(), ProfileRequestError> {
        let request = RestRequest::new(connection_route(connection)?, Priority::UserWrite)
            .json(&serde_json::json!({ "show_activity": show }))?;
        self.execute(request).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/rest/profile-edits.json")).unwrap()
    }

    #[test]
    fn bodies_match_the_fixture() {
        let f = fixture();
        let png = AvatarImage::new(b"\x89PNG\r\n\x1a\nxyz").unwrap();
        assert_eq!(
            account_body(&AccountEdit::DisplayName(" Alt ".into())).unwrap(),
            f["display_name"]
        );
        assert_eq!(
            account_body(&AccountEdit::DisplayName("  ".into())).unwrap(),
            f["display_name_clear"]
        );
        assert_eq!(
            account_body(&AccountEdit::Avatar(Some(png))).unwrap(),
            f["avatar"]
        );
        assert_eq!(
            account_body(&AccountEdit::Avatar(None)).unwrap(),
            f["avatar_clear"]
        );
        let both = ProfileText {
            bio: Some(" Hi ".into()),
            pronouns: Some("they/them".into()),
        };
        assert_eq!(profile_body(&both).unwrap(), f["profile"]);
        let only_bio = ProfileText {
            bio: Some(String::new()),
            pronouns: None,
        };
        assert_eq!(profile_body(&only_bio).unwrap(), f["profile_bio_only"]);
        assert_eq!(nick_body(" Nell ").unwrap(), f["nick"]);
        assert_eq!(nick_body("").unwrap(), f["nick_clear"]);
    }

    #[test]
    fn limits_and_avatar_formats_are_checked_before_anything_is_sent() {
        let long = |n: usize| "x".repeat(n);
        assert_eq!(
            account_body(&AccountEdit::DisplayName(long(MAX_DISPLAY_NAME_CHARS + 1))),
            Err(ProfileError::TooLong)
        );
        assert!(account_body(&AccountEdit::DisplayName(long(MAX_DISPLAY_NAME_CHARS))).is_ok());
        assert_eq!(nick_body(&long(33)), Err(ProfileError::TooLong));
        assert_eq!(
            profile_body(&ProfileText {
                bio: Some(long(MAX_BIO_CHARS + 1)),
                pronouns: None
            }),
            Err(ProfileError::TooLong)
        );
        assert_eq!(
            profile_body(&ProfileText {
                bio: None,
                pronouns: Some(long(MAX_PRONOUNS_CHARS + 1))
            }),
            Err(ProfileError::TooLong)
        );
        assert_eq!(AvatarImage::new(b""), Err(ProfileError::UnsupportedImage));
        // An animated avatar is a paid feature.
        assert_eq!(
            AvatarImage::new(b"GIF89a"),
            Err(ProfileError::UnsupportedImage)
        );
        assert!(AvatarImage::new(&[0xFF, 0xD8, 0xFF, 0xE0]).is_ok());
        let mut big = b"\x89PNG\r\n\x1a\n".to_vec();
        big.resize(crate::image_data::MAX_IMAGE_BYTES + 1, 0);
        assert_eq!(AvatarImage::new(&big), Err(ProfileError::ImageTooLarge));
        let small = AvatarImage::new(b"\x89PNG\r\n\x1a\n").unwrap();
        assert!(!format!("{small:?}").contains("base64"));
    }

    #[test]
    fn connections_decode_leniently_and_route_unsafe_ones_are_not_editable() {
        let body = include_bytes!("../../../fixtures/rest/connections.json");
        let list = parse_connections(body).unwrap();
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].kind, "github");
        assert!(list[0].visible && list[0].editable);
        assert_eq!(list[0].show_activity, Some(false));
        assert_eq!(list[1].show_activity, None);
        assert!(!list[1].visible && list[1].editable);
        assert!(list[2].visible && !list[2].editable);
        assert_eq!(parse_connections(b"{}"), Err(RestError::InvalidJson));
        let route = connection_route(&list[0]).unwrap();
        assert_eq!(route.key().method(), &Method::PATCH);
        assert_eq!(
            route.url.as_str(),
            "https://discord.com/api/v10/users/@me/connections/github/123"
        );
        assert_eq!(
            connection_route(&list[2]).err(),
            Some(ProfileRequestError::Invalid(
                ProfileError::UneditableConnection
            ))
        );
        let many = format!(
            "[{}]",
            vec![r#"{"type":"x","id":"1"}"#; MAX_CONNECTIONS + 5].join(",")
        );
        assert_eq!(
            parse_connections(many.as_bytes()).unwrap().len(),
            MAX_CONNECTIONS
        );
    }

    #[test]
    fn the_nickname_route_names_the_guild_and_me() {
        let route = Route::new(Method::PATCH, "/guilds/9/members/@me").unwrap();
        assert_eq!(
            route.url.as_str(),
            "https://discord.com/api/v10/guilds/9/members/@me"
        );
        assert_eq!(route.key().method(), &Method::PATCH);
    }
}
