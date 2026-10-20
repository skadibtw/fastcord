//! Group DM management requests (SPEC §16, issue #78): add or remove one
//! recipient, rename, set or clear the icon, transfer ownership, and close a
//! DM or leave a group. Each function sends exactly one request on the user's
//! click and never retries; the conversation changes only when Discord's
//! `CHANNEL_UPDATE`, `CHANNEL_RECIPIENT_*` or `CHANNEL_DELETE` arrives.
//! Unverified against a live account (`docs/TESTING.md`).

use base64::Engine;
use fastcord_model::Snowflake;
use reqwest::Method;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Longest group name Discord accepts.
pub const MAX_GROUP_NAME_CHARS: usize = 100;
/// Largest icon image read. The base64 form must fit the 1 MiB request budget.
pub const MAX_ICON_BYTES: usize = 512 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupError {
    NameTooLong,
    /// Not a PNG, JPEG or GIF, or empty.
    UnsupportedImage,
    ImageTooLarge,
}

/// A validated icon, already in the `data:` form the PATCH body carries.
#[derive(Clone, PartialEq, Eq)]
pub struct IconImage(String);

impl std::fmt::Debug for IconImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IconImage({} bytes)", self.0.len())
    }
}

impl IconImage {
    /// Accepts a PNG, JPEG or GIF of at most [`MAX_ICON_BYTES`], judged by its
    /// signature rather than its file name.
    pub fn new(bytes: &[u8]) -> Result<Self, GroupError> {
        let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
            "image/jpeg"
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            "image/gif"
        } else {
            return Err(GroupError::UnsupportedImage);
        };
        if bytes.len() > MAX_ICON_BYTES {
            return Err(GroupError::ImageTooLarge);
        }
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        Ok(Self(format!("data:{mime};base64,{encoded}")))
    }
}

/// One change to the conversation itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupEdit {
    /// An empty name restores the default (the members' names).
    Name(String),
    /// `None` removes the icon.
    Icon(Option<IconImage>),
    /// Hand the group to another member (owner only).
    Owner(Snowflake),
}

fn edit_body(edit: &GroupEdit) -> Result<serde_json::Value, GroupError> {
    Ok(match edit {
        GroupEdit::Name(name) => {
            let name = name.trim();
            if name.chars().count() > MAX_GROUP_NAME_CHARS {
                return Err(GroupError::NameTooLong);
            }
            serde_json::json!({ "name": (!name.is_empty()).then_some(name) })
        }
        GroupEdit::Icon(icon) => serde_json::json!({ "icon": icon.as_ref().map(|i| &i.0) }),
        GroupEdit::Owner(user) => serde_json::json!({ "owner": user.to_string() }),
    })
}

fn recipient_route(
    method: Method,
    channel: Snowflake,
    user: Snowflake,
) -> Result<Route, RestError> {
    Route::new(method, &format!("/channels/{channel}/recipients/{user}"))
}

fn close_route(channel: Snowflake, silent: bool) -> Result<Route, RestError> {
    let query = if silent { "?silent=true" } else { "" };
    Route::new(Method::DELETE, &format!("/channels/{channel}{query}"))
}

/// A failed edit: invalid input (nothing sent) or Discord's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupRequestError {
    Invalid(GroupError),
    Rest(RestError),
}

impl From<RestError> for GroupRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

impl<C: Clock> RestClient<C> {
    /// Adds one account to a group DM (`PUT /channels/{id}/recipients/{user}`).
    pub async fn add_group_recipient(
        &self,
        channel: Snowflake,
        user: Snowflake,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(
            recipient_route(Method::PUT, channel, user)?,
            Priority::UserWrite,
        );
        self.execute(request).await.map(drop)
    }

    /// Removes one account from a group DM the account owns.
    pub async fn remove_group_recipient(
        &self,
        channel: Snowflake,
        user: Snowflake,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(
            recipient_route(Method::DELETE, channel, user)?,
            Priority::UserWrite,
        );
        self.execute(request).await.map(drop)
    }

    /// Renames, re-icons or hands over a group DM (`PATCH /channels/{id}`).
    pub async fn edit_group(
        &self,
        channel: Snowflake,
        edit: &GroupEdit,
    ) -> Result<(), GroupRequestError> {
        let body = edit_body(edit).map_err(GroupRequestError::Invalid)?;
        let request = RestRequest::new(
            Route::new(Method::PATCH, &format!("/channels/{channel}"))?,
            Priority::UserWrite,
        )
        .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Closes a DM or leaves a group DM (`DELETE /channels/{id}`). A DM
    /// reappears when opened again; a group needs a new invitation.
    pub async fn close_private_channel(
        &self,
        channel: Snowflake,
        silent: bool,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(close_route(channel, silent)?, Priority::UserWrite);
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

    #[test]
    fn routes_name_the_channel_and_the_one_user() {
        let add = recipient_route(Method::PUT, Snowflake(5), Snowflake(6)).unwrap();
        assert_eq!(path(&add), "/channels/5/recipients/6");
        assert_eq!(add.key().method(), &Method::PUT);
        let remove = recipient_route(Method::DELETE, Snowflake(5), Snowflake(6)).unwrap();
        assert_eq!(remove.key().method(), &Method::DELETE);
        assert_eq!(
            path(&close_route(Snowflake(5), false).unwrap()),
            "/channels/5"
        );
        assert_eq!(
            path(&close_route(Snowflake(5), true).unwrap()),
            "/channels/5?silent=true"
        );
    }

    #[test]
    fn edit_bodies_match_the_fixture() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/rest/group-dm-edits.json"))
                .unwrap();
        let png = IconImage::new(b"\x89PNG\r\n\x1a\nxyz").unwrap();
        assert_eq!(
            edit_body(&GroupEdit::Name(" Team ".into())).unwrap(),
            fixture["name"]
        );
        assert_eq!(
            edit_body(&GroupEdit::Name("  ".into())).unwrap(),
            fixture["name_reset"]
        );
        assert_eq!(
            edit_body(&GroupEdit::Icon(Some(png))).unwrap(),
            fixture["icon"]
        );
        assert_eq!(
            edit_body(&GroupEdit::Icon(None)).unwrap(),
            fixture["icon_clear"]
        );
        assert_eq!(
            edit_body(&GroupEdit::Owner(Snowflake(77))).unwrap(),
            fixture["owner"]
        );
    }

    #[test]
    fn names_and_icons_are_validated_before_anything_is_sent() {
        assert_eq!(
            edit_body(&GroupEdit::Name("x".repeat(MAX_GROUP_NAME_CHARS + 1))),
            Err(GroupError::NameTooLong)
        );
        assert!(edit_body(&GroupEdit::Name("x".repeat(MAX_GROUP_NAME_CHARS))).is_ok());
        assert_eq!(IconImage::new(b""), Err(GroupError::UnsupportedImage));
        assert_eq!(IconImage::new(b"<svg/>"), Err(GroupError::UnsupportedImage));
        let mut big = b"GIF89a".to_vec();
        big.resize(MAX_ICON_BYTES + 1, 0);
        assert_eq!(IconImage::new(&big), Err(GroupError::ImageTooLarge));
        big.truncate(MAX_ICON_BYTES);
        let icon = IconImage::new(&big).unwrap();
        // The largest accepted icon still fits the request body budget.
        assert!(icon.0.len() < 1024 * 1024);
        assert!(IconImage::new(&[0xFF, 0xD8, 0xFF, 0xE0]).is_ok());
        assert!(!format!("{icon:?}").contains("base64"));
    }
}
