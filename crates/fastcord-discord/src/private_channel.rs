//! Explicitly selected private conversation listing and creation (SPEC §5.2).

use fastcord_model::{Channel, Snowflake};
use reqwest::Method;
use serde::Serialize;

use crate::{Priority, RestClient, RestError, RestRequest, Route};

/// Discord group DMs have at most ten members including the current account.
/// The explicit recipient list therefore contains at most nine other accounts.
pub const MAX_PRIVATE_RECIPIENTS: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateChannelError {
    InvalidRecipients,
    InvalidResponse,
    Rest(RestError),
}

impl From<RestError> for PrivateChannelError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

#[derive(Serialize)]
struct CreatePrivateChannel<'a> {
    recipients: &'a [Snowflake],
}

impl<C: crate::Clock> RestClient<C> {
    /// Lists private channels using the documented account-scoped endpoint.
    pub async fn private_channels(&self) -> Result<Vec<Channel>, RestError> {
        self.get("/users/@me/channels", Priority::UserRead).await
    }

    /// Opens an existing DM or creates a DM/group DM for only the supplied IDs.
    /// This performs exactly one POST and never discovers recipients.
    pub async fn open_private_channel(
        &self,
        recipients: &[Snowflake],
    ) -> Result<Channel, PrivateChannelError> {
        if recipients.is_empty() || recipients.len() > MAX_PRIVATE_RECIPIENTS {
            return Err(PrivateChannelError::InvalidRecipients);
        }
        for (index, recipient) in recipients.iter().enumerate() {
            if recipients[..index].contains(recipient) {
                return Err(PrivateChannelError::InvalidRecipients);
            }
        }
        let request = RestRequest::new(
            Route::new(Method::POST, "/users/@me/channels")?,
            Priority::UserWrite,
        )
        .json(&CreatePrivateChannel { recipients })?;
        let channel: Channel = self.execute(request).await?.json()?;
        if channel.guild_id.is_some()
            || !matches!(
                channel.kind,
                fastcord_model::ChannelKind::Dm | fastcord_model::ChannelKind::GroupDm
            )
        {
            return Err(PrivateChannelError::InvalidResponse);
        }
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserToken;

    #[tokio::test]
    async fn open_requires_a_bounded_unique_explicit_recipient_selection() {
        let client = RestClient::new(UserToken::new("dummy".into())).unwrap();
        assert_eq!(
            client.open_private_channel(&[]).await,
            Err(PrivateChannelError::InvalidRecipients)
        );
        assert_eq!(
            client
                .open_private_channel(&[Snowflake(1); MAX_PRIVATE_RECIPIENTS + 1])
                .await,
            Err(PrivateChannelError::InvalidRecipients)
        );
        assert_eq!(
            client
                .open_private_channel(&[Snowflake(1), Snowflake(1)])
                .await,
            Err(PrivateChannelError::InvalidRecipients)
        );
    }

    #[test]
    fn create_request_matches_the_sanitized_fixture_and_response_is_a_group_dm() {
        let body = serde_json::to_value(CreatePrivateChannel {
            recipients: &[Snowflake(42), Snowflake(43)],
        })
        .unwrap();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/rest/private-channel-create-request.json"
        ))
        .unwrap();
        assert_eq!(body, fixture);

        let channel: Channel = serde_json::from_str(include_str!(
            "../../../fixtures/rest/private-channel-group-response.json"
        ))
        .unwrap();
        assert_eq!(channel.kind, fastcord_model::ChannelKind::GroupDm);
        assert!(channel.guild_id.is_none());
        assert_eq!(
            channel
                .recipients
                .iter()
                .map(|user| user.id)
                .collect::<Vec<_>>(),
            [Snowflake(42), Snowflake(43)]
        );
    }
}
