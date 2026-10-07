use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use fastcord_model::Snowflake;
use reqwest::{Method, Url};
use zeroize::Zeroizing;

use crate::RestError;

pub(crate) const API_BASE: &str = "https://discord.com/api/v10";
const MAX_ROUTE_BYTES: usize = 8 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct WebhookToken(Arc<Zeroizing<String>>);

impl Hash for WebhookToken {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.as_str().hash(state);
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum MajorKind {
    None,
    Channel(Snowflake),
    Guild(Snowflake),
    Webhook(Snowflake, Option<WebhookToken>),
}

/// Major resource identity is retained even when Discord shares a bucket hash.
/// Webhook tokens participate in equality but are never printed by Debug.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct MajorParameter(MajorKind);

impl MajorParameter {
    pub fn channel_id(&self) -> Option<Snowflake> {
        match self.0 {
            MajorKind::Channel(id) => Some(id),
            _ => None,
        }
    }

    pub fn guild_id(&self) -> Option<Snowflake> {
        match self.0 {
            MajorKind::Guild(id) => Some(id),
            _ => None,
        }
    }

    pub fn webhook_id(&self) -> Option<Snowflake> {
        match self.0 {
            MajorKind::Webhook(id, _) => Some(id),
            _ => None,
        }
    }

    fn secret_bytes(&self) -> usize {
        match &self.0 {
            MajorKind::Webhook(_, Some(token)) => token.0.len(),
            _ => 0,
        }
    }
}

impl fmt::Debug for MajorParameter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            MajorKind::None => f.write_str("None"),
            MajorKind::Channel(id) => f.debug_tuple("Channel").field(id).finish(),
            MajorKind::Guild(id) => f.debug_tuple("Guild").field(id).finish(),
            MajorKind::Webhook(id, token) => f
                .debug_struct("Webhook")
                .field("id", id)
                .field("token", &token.as_ref().map(|_| "[REDACTED]"))
                .finish(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RouteKey {
    method: Method,
    normalized: Arc<str>,
    major: MajorParameter,
}

impl RouteKey {
    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn normalized(&self) -> &str {
        &self.normalized
    }

    pub fn major(&self) -> &MajorParameter {
        &self.major
    }

    pub(crate) fn tracked_bytes(&self) -> usize {
        // Includes hash-table slack, bucket state, and its learned-hash entry.
        // Overcount shared strings rather than undercount the byte budget.
        1024 + self.normalized.len() + self.major.secret_bytes() + self.method.as_str().len()
    }
}

/// A resource path under the fixed `/api/v10` origin, never an arbitrary URL.
/// Queries affect the request, not its rate-limit key.
#[derive(Clone)]
pub struct Route {
    pub(crate) key: RouteKey,
    pub(crate) url: Url,
}

impl Route {
    pub fn new(method: Method, path: &str) -> Result<Self, RestError> {
        if !path.starts_with('/')
            || path.starts_with("//")
            || path.len() > MAX_ROUTE_BYTES
            || path.contains(['\\', '#'])
            || path.bytes().any(|b| b.is_ascii_control() || b == b' ')
        {
            return Err(RestError::InvalidRoute);
        }
        let resource = path.split('?').next().ok_or(RestError::InvalidRoute)?;
        if resource.split('/').skip(1).any(|segment| {
            segment.is_empty()
                || [".", "..", "%2e", ".%2e", "%2e.", "%2e%2e"]
                    .iter()
                    .any(|dots| segment.eq_ignore_ascii_case(dots))
        }) {
            return Err(RestError::InvalidRoute);
        }
        let url = Url::parse(&format!("{API_BASE}{path}")).map_err(|_| RestError::InvalidRoute)?;
        if url.scheme() != "https"
            || url.host_str() != Some("discord.com")
            || url.port().is_some()
            || !url.path().starts_with("/api/v10/")
        {
            return Err(RestError::InvalidRoute);
        }
        let segments = url.path()["/api/v10/".len()..].split('/');
        let mut head = segments.clone();
        let family = head.next().unwrap_or_default();
        let major_id = head.next().and_then(|id| id.parse::<Snowflake>().ok());
        let webhook_token = if family == "webhooks" && major_id.is_some() {
            head.next()
                .map(|token| WebhookToken(Arc::new(Zeroizing::new(token.to_owned()))))
        } else {
            None
        };
        let major = match (family, major_id) {
            ("channels", Some(id)) => MajorKind::Channel(id),
            ("guilds", Some(id)) => MajorKind::Guild(id),
            ("webhooks", Some(id)) => MajorKind::Webhook(id, webhook_token),
            _ => MajorKind::None,
        };
        let mut normalized = String::with_capacity(url.path().len());
        let mut previous = "";
        for (index, segment) in segments.enumerate() {
            normalized.push('/');
            let variable = if index == 1 && major_id.is_some() {
                match family {
                    "channels" => Some("{channel_id}"),
                    "guilds" => Some("{guild_id}"),
                    "webhooks" => Some("{webhook_id}"),
                    _ => Some("{id}"),
                }
            } else if index == 2 && matches!(major, MajorKind::Webhook(_, Some(_))) {
                Some("{webhook_token}")
            } else if previous == "reactions" {
                Some("{emoji}")
            } else if segment.parse::<Snowflake>().is_ok() {
                Some("{id}")
            } else {
                None
            };
            normalized.push_str(variable.unwrap_or(segment));
            previous = segment;
        }
        Ok(Self {
            key: RouteKey {
                method,
                normalized: normalized.into(),
                major: MajorParameter(major),
            },
            url,
        })
    }

    pub fn key(&self) -> &RouteKey {
        &self.key
    }
}

impl fmt::Debug for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Route").field("key", &self.key).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_minor_ids_and_queries_but_preserves_method_and_major() {
        let first = Route::new(Method::GET, "/channels/123/messages/456?limit=50").unwrap();
        let second = Route::new(Method::GET, "/channels/123/messages/789?limit=10").unwrap();
        assert_eq!(first.key(), second.key());
        assert_eq!(
            first.key().normalized(),
            "/channels/{channel_id}/messages/{id}"
        );
        assert_eq!(first.key().major().channel_id(), Some(Snowflake(123)));
        assert_ne!(
            first.key(),
            Route::new(Method::DELETE, "/channels/123/messages/456")
                .unwrap()
                .key()
        );
        assert_ne!(
            first.key(),
            Route::new(Method::GET, "/channels/124/messages/456")
                .unwrap()
                .key()
        );
        assert_eq!(
            first.url.as_str(),
            "https://discord.com/api/v10/channels/123/messages/456?limit=50"
        );
    }

    #[test]
    fn reaction_values_normalize_and_webhook_credentials_are_redacted() {
        let first = Route::new(Method::PUT, "/channels/1/messages/2/reactions/a%3A3/@me").unwrap();
        let second = Route::new(
            Method::PUT,
            "/channels/1/messages/4/reactions/%F0%9F%91%8D/@me",
        )
        .unwrap();
        assert_eq!(first.key(), second.key());
        let webhook = Route::new(
            Method::GET,
            "/webhooks/5/offline-test-credential/messages/6",
        )
        .unwrap();
        assert_eq!(webhook.key().major().webhook_id(), Some(Snowflake(5)));
        assert!(!format!("{webhook:?}").contains("offline-test-credential"));
        assert_ne!(
            webhook.key(),
            Route::new(Method::GET, "/webhooks/5/different-credential/messages/6")
                .unwrap()
                .key()
        );
    }

    #[test]
    fn refuses_other_origins_fragments_and_path_traversal() {
        for path in [
            "https://example.com",
            "//example.com/a",
            "/../users/@me",
            "/%2e%2e/users/@me",
            "/channels/1/../../users/@me",
            "/users/@me#token",
            "/users\\@me",
            "/users//a",
        ] {
            assert!(
                matches!(Route::new(Method::GET, path), Err(RestError::InvalidRoute)),
                "{path}"
            );
        }
    }
}
