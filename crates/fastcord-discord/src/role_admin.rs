//! Roles and channel access management (SPEC §16, issue #94): create, edit,
//! delete and reorder roles, give or take a role of a member, write or remove
//! one channel permission overwrite, and copy a category's overwrites onto a
//! channel. Each function sends exactly one request on the user's click and
//! never retries; roles, members and overwrites change only when Discord's
//! `GUILD_ROLE_*`, `GUILD_MEMBER_UPDATE` and `CHANNEL_UPDATE` arrive. The
//! hierarchy and grant rules are decided on the reducer (`GuildAdmin`) before
//! a click reaches here; Discord stays the authority. Role icons, gradient and
//! holographic colors and other premium role styles are not offered. Wire
//! shapes come from community documentation of the official client and are
//! unverified against a live account (`docs/TESTING.md`).

use fastcord_model::{OverwriteKind, PermissionOverwrite, Permissions, Snowflake};
use reqwest::Method;

use crate::guild_admin::placements_body;
use crate::{
    AdminError, AdminRequestError, Clock, Placement, Priority, RestClient, RestError, RestRequest,
    Route, WriteGroup,
};

pub const MAX_ROLE_NAME_CHARS: usize = 100;
/// The largest plain RGB colour.
pub const MAX_ROLE_COLOR: u32 = 0x00FF_FFFF;
/// Overwrites Discord keeps per channel.
pub const MAX_OVERWRITES: usize = 100;

/// Changes to one role; `None` leaves a field as it is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleEdit {
    pub name: Option<String>,
    pub color: Option<u32>,
    pub hoist: Option<bool>,
    pub mentionable: Option<bool>,
    pub permissions: Option<Permissions>,
}

fn role_body(edit: &RoleEdit) -> Result<serde_json::Map<String, serde_json::Value>, AdminError> {
    let mut body = serde_json::Map::new();
    if let Some(name) = &edit.name {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > MAX_ROLE_NAME_CHARS {
            return Err(AdminError::BadName);
        }
        body.insert("name".into(), name.into());
    }
    if let Some(color) = edit.color {
        if color > MAX_ROLE_COLOR {
            return Err(AdminError::BadValue);
        }
        body.insert("color".into(), color.into());
    }
    if let Some(hoist) = edit.hoist {
        body.insert("hoist".into(), hoist.into());
    }
    if let Some(mentionable) = edit.mentionable {
        body.insert("mentionable".into(), mentionable.into());
    }
    if let Some(permissions) = edit.permissions {
        body.insert("permissions".into(), permissions.0.to_string().into());
    }
    Ok(body)
}

fn edit_role_body(edit: &RoleEdit) -> Result<serde_json::Value, AdminError> {
    let body = role_body(edit)?;
    if body.is_empty() {
        return Err(AdminError::NothingToChange);
    }
    Ok(body.into())
}

/// A new role needs a name; the other fields default on Discord's side.
fn new_role_body(new: &RoleEdit) -> Result<serde_json::Value, AdminError> {
    if new.name.is_none() {
        return Err(AdminError::BadName);
    }
    Ok(role_body(new)?.into())
}

/// One overwrite as a channel edit carries it.
fn overwrite_json(overwrite: &PermissionOverwrite) -> serde_json::Value {
    serde_json::json!({
        "id": overwrite.id.to_string(),
        "type": overwrite.kind as u8,
        "allow": overwrite.allow.0.to_string(),
        "deny": overwrite.deny.0.to_string(),
    })
}

fn overwrite_body(
    kind: OverwriteKind,
    allow: Permissions,
    deny: Permissions,
) -> Result<serde_json::Value, AdminError> {
    // A bit cannot be allowed and denied at once.
    if allow.0 & deny.0 != 0 {
        return Err(AdminError::BadValue);
    }
    Ok(serde_json::json!({
        "type": kind as u8,
        "allow": allow.0.to_string(),
        "deny": deny.0.to_string(),
    }))
}

fn sync_body(overwrites: &[PermissionOverwrite]) -> Result<serde_json::Value, AdminError> {
    if overwrites.len() > MAX_OVERWRITES {
        return Err(AdminError::BadValue);
    }
    Ok(serde_json::json!({
        "permission_overwrites": overwrites.iter().map(overwrite_json).collect::<Vec<_>>(),
    }))
}

fn write(method: Method, path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(Route::new(method, path)?, Priority::UserWrite).with_error_detail())
}

impl<C: Clock> RestClient<C> {
    /// Creates a role (`POST /guilds/{id}/roles`); it appears with
    /// `GUILD_ROLE_CREATE`.
    pub async fn create_role(
        &self,
        guild: Snowflake,
        new: &RoleEdit,
    ) -> Result<(), AdminRequestError> {
        let body = new_role_body(new).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::POST, &format!("/guilds/{guild}/roles"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Changes one role (`PATCH /guilds/{id}/roles/{role}`); the display
    /// follows `GUILD_ROLE_UPDATE`.
    pub async fn edit_role(
        &self,
        guild: Snowflake,
        role: Snowflake,
        edit: &RoleEdit,
    ) -> Result<(), AdminRequestError> {
        let body = edit_role_body(edit).map_err(AdminRequestError::Invalid)?;
        let request =
            write(Method::PATCH, &format!("/guilds/{guild}/roles/{role}"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Deletes a role (`DELETE /guilds/{id}/roles/{role}`).
    pub async fn delete_role(&self, guild: Snowflake, role: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/guilds/{guild}/roles/{role}"))?;
        self.execute(request).await.map(drop)
    }

    /// Moves roles (`PATCH /guilds/{id}/roles` with `[{id, position}]`); each
    /// moved role arrives as a `GUILD_ROLE_UPDATE`.
    pub async fn reorder_roles(
        &self,
        guild: Snowflake,
        placements: &[Placement],
    ) -> Result<(), AdminRequestError> {
        let body = placements_body(placements).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::PATCH, &format!("/guilds/{guild}/roles"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Gives (`PUT`) or takes (`DELETE`) one role of a member
    /// (`/guilds/{id}/members/{user}/roles/{role}`); the member changes with
    /// `GUILD_MEMBER_UPDATE`.
    pub async fn set_member_role(
        &self,
        guild: Snowflake,
        user: Snowflake,
        role: Snowflake,
        give: bool,
    ) -> Result<(), RestError> {
        let method = if give { Method::PUT } else { Method::DELETE };
        let request = write(
            method,
            &format!("/guilds/{guild}/members/{user}/roles/{role}"),
        )?;
        self.execute(request).await.map(drop)
    }

    /// Sets what a role or member is allowed and denied in a channel
    /// (`PUT /channels/{id}/permissions/{target}`); the channel changes with
    /// `CHANNEL_UPDATE`.
    pub async fn put_overwrite(
        &self,
        channel: Snowflake,
        target: Snowflake,
        kind: OverwriteKind,
        allow: Permissions,
        deny: Permissions,
    ) -> Result<(), AdminRequestError> {
        let body = overwrite_body(kind, allow, deny).map_err(AdminRequestError::Invalid)?;
        let request = write(
            Method::PUT,
            &format!("/channels/{channel}/permissions/{target}"),
        )?
        .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Removes one overwrite (`DELETE /channels/{id}/permissions/{target}`).
    pub async fn delete_overwrite(
        &self,
        channel: Snowflake,
        target: Snowflake,
    ) -> Result<(), RestError> {
        let request = write(
            Method::DELETE,
            &format!("/channels/{channel}/permissions/{target}"),
        )?;
        self.execute(request).await.map(drop)
    }

    /// Makes a channel's overwrites those of its category
    /// (`PATCH /channels/{id}` with the category's `permission_overwrites`).
    pub async fn sync_overwrites(
        &self,
        channel: Snowflake,
        category: &[PermissionOverwrite],
    ) -> Result<(), AdminRequestError> {
        let body = sync_body(category).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::PATCH, &format!("/channels/{channel}"))?
            .in_group(WriteGroup::Roles)
            .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_role_edit_sends_only_what_changed() {
        let edit = RoleEdit {
            name: Some(" Mods ".into()),
            color: Some(0x3498DB),
            hoist: Some(true),
            mentionable: Some(false),
            permissions: Some(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES),
        };
        assert_eq!(
            edit_role_body(&edit).unwrap(),
            json!({"name": "Mods", "color": 3_447_003, "hoist": true,
                   "mentionable": false, "permissions": "3072"})
        );
        assert_eq!(
            edit_role_body(&RoleEdit::default()),
            Err(AdminError::NothingToChange)
        );
        for bad in [
            RoleEdit {
                name: Some("  ".into()),
                ..RoleEdit::default()
            },
            RoleEdit {
                name: Some("x".repeat(101)),
                ..RoleEdit::default()
            },
            RoleEdit {
                color: Some(MAX_ROLE_COLOR + 1),
                ..RoleEdit::default()
            },
        ] {
            assert!(edit_role_body(&bad).is_err());
        }
    }

    #[test]
    fn a_new_role_needs_a_name() {
        assert_eq!(
            new_role_body(&RoleEdit::default()),
            Err(AdminError::BadName)
        );
        let new = RoleEdit {
            name: Some("Helpers".into()),
            ..RoleEdit::default()
        };
        assert_eq!(new_role_body(&new).unwrap(), json!({"name": "Helpers"}));
    }

    #[test]
    fn an_overwrite_has_a_type_and_string_masks_and_no_shared_bit() {
        let allow = Permissions::VIEW_CHANNEL;
        let deny = Permissions::SEND_MESSAGES;
        assert_eq!(
            overwrite_body(OverwriteKind::Member, allow, deny).unwrap(),
            json!({"type": 1, "allow": "1024", "deny": "2048"})
        );
        assert_eq!(
            overwrite_body(OverwriteKind::Role, allow, allow),
            Err(AdminError::BadValue)
        );
    }

    #[test]
    fn a_sync_carries_the_categorys_overwrites_and_is_bounded() {
        let overwrite = PermissionOverwrite {
            id: Snowflake(5),
            kind: OverwriteKind::Role,
            allow: Permissions::NONE,
            deny: Permissions::VIEW_CHANNEL,
        };
        assert_eq!(
            sync_body(std::slice::from_ref(&overwrite)).unwrap(),
            json!({"permission_overwrites": [
                {"id": "5", "type": 0, "allow": "0", "deny": "1024"}
            ]})
        );
        assert_eq!(
            sync_body(&[]).unwrap(),
            json!({"permission_overwrites": []})
        );
        assert!(sync_body(&vec![overwrite; MAX_OVERWRITES + 1]).is_err());
    }
}
