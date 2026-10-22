//! Communication app access (SPEC §16, issue #101): the apps the account has
//! authorized (`GET /oauth2/tokens`, `DELETE /oauth2/tokens/{id}`), a server's
//! integrations (`GET /guilds/{g}/integrations`,
//! `DELETE /guilds/{g}/integrations/{id}`) and the incoming webhooks of a
//! channel (`GET`/`POST /channels/{c}/webhooks`, `PATCH`/`DELETE /webhooks/{id}`). Each write is one request on the user's
//! click and is never retried. No authorization is ever granted here: adding an
//! app opens Discord's own consent page in the browser ([`authorize_url`]).
//! A webhook `token` is parsed into [`WebhookToken`], which is redacted in
//! `Debug`, zeroized on drop and printed only by [`WebhookToken::url`]. Every
//! route and wire shape is community-documented and unverified against a live
//! account (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde::Deserialize;
use zeroize::Zeroizing;

use crate::invite::cut_at;
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Rows kept from one answer.
pub const MAX_ROWS: usize = 100;
const MAX_TEXT_CHARS: usize = 100;
const MAX_SCOPES: usize = 24;
/// Longest webhook name Discord accepts.
pub const MAX_WEBHOOK_NAME: usize = 80;
/// `type` of an incoming webhook.
const INCOMING: u8 = 1;

/// A webhook's secret. Never in `Debug`, `Display`, logs or fixtures.
#[derive(Clone, PartialEq, Eq)]
pub struct WebhookToken(Zeroizing<String>);

impl std::fmt::Debug for WebhookToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookToken([REDACTED])")
    }
}

impl WebhookToken {
    /// The URL a webhook is executed at. Handed out only for an explicit copy.
    pub fn url(&self, id: Snowflake) -> String {
        format!("https://discord.com/api/webhooks/{id}/{}", self.0.as_str())
    }
}

/// One app the account authorized.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedApp {
    /// The authorization id used to deauthorize.
    pub id: Snowflake,
    pub name: String,
    pub scopes: Vec<String>,
    /// Discord's ISO 8601 text, as sent.
    pub authorized_at: Option<String>,
}

/// One integration or bot installed in a server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Integration {
    pub id: Snowflake,
    pub name: String,
    pub kind: String,
    pub account: String,
    /// The bot user, when the integration has one.
    pub bot: Option<String>,
}

/// An incoming webhook of a channel.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Webhook {
    pub id: Snowflake,
    pub name: String,
    pub channel: Snowflake,
    pub token: Option<WebhookToken>,
}

fn text(value: Option<String>) -> String {
    cut_at(value.unwrap_or_default(), MAX_TEXT_CHARS)
}

#[derive(Deserialize)]
struct WireName {
    #[serde(default)]
    name: Option<String>,
}

pub fn parse_authorized(body: &[u8]) -> Option<Vec<AuthorizedApp>> {
    #[derive(Deserialize)]
    struct Wire {
        id: Snowflake,
        #[serde(default)]
        application: Option<WireName>,
        #[serde(default)]
        scopes: Vec<String>,
        #[serde(default)]
        authorized_at: Option<String>,
    }
    let wire: Vec<Wire> = serde_json::from_slice(body).ok()?;
    Some(
        wire.into_iter()
            .take(MAX_ROWS)
            .map(|w| AuthorizedApp {
                id: w.id,
                name: text(w.application.and_then(|a| a.name)),
                scopes: w
                    .scopes
                    .into_iter()
                    .take(MAX_SCOPES)
                    .map(|s| cut_at(s, 40))
                    .collect(),
                authorized_at: w.authorized_at.map(|t| cut_at(t, 40)),
            })
            .collect(),
    )
}

pub fn parse_integrations(body: &[u8]) -> Option<Vec<Integration>> {
    #[derive(Deserialize)]
    struct Wire {
        id: Snowflake,
        #[serde(default)]
        name: Option<String>,
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        account: Option<WireName>,
        #[serde(default)]
        application: Option<WireApp>,
    }
    #[derive(Deserialize)]
    struct WireApp {
        #[serde(default)]
        bot: Option<WireBot>,
    }
    #[derive(Deserialize)]
    struct WireBot {
        #[serde(default)]
        username: Option<String>,
    }
    let wire: Vec<Wire> = serde_json::from_slice(body).ok()?;
    Some(
        wire.into_iter()
            .take(MAX_ROWS)
            .map(|w| Integration {
                id: w.id,
                name: text(w.name),
                kind: text(w.kind),
                account: text(w.account.and_then(|a| a.name)),
                bot: w
                    .application
                    .and_then(|a| a.bot)
                    .and_then(|b| b.username)
                    .map(|n| cut_at(n, MAX_TEXT_CHARS)),
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct WireWebhook {
    id: Snowflake,
    #[serde(rename = "type", default)]
    kind: u8,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    channel_id: Option<Snowflake>,
    #[serde(default)]
    token: Option<String>,
}

fn webhook(w: WireWebhook) -> Option<Webhook> {
    Some(Webhook {
        id: w.id,
        name: text(w.name),
        channel: w.channel_id?,
        token: w
            .token
            .filter(|t| !t.is_empty())
            .map(|t| WebhookToken(t.into())),
    })
}

/// Incoming webhooks only (type 1) of a `GET /channels/{id}/webhooks` answer.
pub fn parse_webhooks(body: &[u8]) -> Option<Vec<Webhook>> {
    let wire: Vec<WireWebhook> = serde_json::from_slice(body).ok()?;
    Some(
        wire.into_iter()
            .filter(|w| w.kind == INCOMING)
            .take(MAX_ROWS)
            .filter_map(webhook)
            .collect(),
    )
}

/// One webhook, as `POST`/`PATCH` answer.
pub fn parse_webhook(body: &[u8]) -> Option<Webhook> {
    webhook(serde_json::from_slice(body).ok()?)
}

/// A client id typed by the user: decimal digits only.
pub fn parse_client_id(input: &str) -> Option<Snowflake> {
    let input = input.trim();
    if input.is_empty() || input.len() > 20 || !input.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    input.parse().ok()
}

/// Discord's own consent page for adding an app to a server. Consent happens
/// there; fastcord never authorizes.
pub fn authorize_url(client: Snowflake) -> String {
    format!(
        "https://discord.com/oauth2/authorize?client_id={client}&scope=bot%20applications.commands"
    )
}

fn write(method: Method, path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(Route::new(method, path)?, Priority::UserWrite).with_error_detail())
}

fn read(path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(
        Route::new(Method::GET, path)?,
        Priority::UserRead,
    ))
}

fn name_ok(name: &str) -> Result<&str, RestError> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > MAX_WEBHOOK_NAME {
        Err(RestError::InvalidJson)
    } else {
        Ok(name)
    }
}

fn webhook_edit_body(
    name: Option<&str>,
    channel: Option<Snowflake>,
) -> Result<serde_json::Value, RestError> {
    let mut body = serde_json::Map::new();
    if let Some(name) = name {
        body.insert("name".into(), name_ok(name)?.into());
    }
    if let Some(channel) = channel {
        body.insert("channel_id".into(), channel.to_string().into());
    }
    if body.is_empty() {
        return Err(RestError::InvalidJson);
    }
    Ok(body.into())
}

impl<C: Clock> RestClient<C> {
    pub async fn authorized_apps(&self) -> Result<Vec<AuthorizedApp>, RestError> {
        parse_authorized(self.execute(read("/oauth2/tokens")?).await?.body())
            .ok_or(RestError::InvalidJson)
    }

    pub async fn deauthorize_app(&self, id: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/oauth2/tokens/{id}"))?;
        self.execute(request).await.map(drop)
    }

    pub async fn integrations(&self, guild: Snowflake) -> Result<Vec<Integration>, RestError> {
        let request = read(&format!("/guilds/{guild}/integrations"))?;
        parse_integrations(self.execute(request).await?.body()).ok_or(RestError::InvalidJson)
    }

    pub async fn delete_integration(
        &self,
        guild: Snowflake,
        id: Snowflake,
    ) -> Result<(), RestError> {
        let request = write(
            Method::DELETE,
            &format!("/guilds/{guild}/integrations/{id}"),
        )?;
        self.execute(request).await.map(drop)
    }

    pub async fn channel_webhooks(&self, channel: Snowflake) -> Result<Vec<Webhook>, RestError> {
        let request = read(&format!("/channels/{channel}/webhooks"))?;
        parse_webhooks(self.execute(request).await?.body()).ok_or(RestError::InvalidJson)
    }

    pub async fn create_webhook(
        &self,
        channel: Snowflake,
        name: &str,
    ) -> Result<Webhook, RestError> {
        let request = write(Method::POST, &format!("/channels/{channel}/webhooks"))?
            .json(&serde_json::json!({ "name": name_ok(name)? }))?;
        parse_webhook(self.execute(request).await?.body()).ok_or(RestError::InvalidJson)
    }

    /// Renames and/or moves a webhook; the answer carries the new state.
    pub async fn edit_webhook(
        &self,
        webhook: Snowflake,
        name: Option<&str>,
        channel: Option<Snowflake>,
    ) -> Result<Webhook, RestError> {
        let body = webhook_edit_body(name, channel)?;
        let request = write(Method::PATCH, &format!("/webhooks/{webhook}"))?.json(&body)?;
        parse_webhook(self.execute(request).await?.body()).ok_or(RestError::InvalidJson)
    }

    pub async fn delete_webhook(&self, webhook: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/webhooks/{webhook}"))?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTHORIZED: &str = include_str!("../../../fixtures/rest/oauth2-tokens-synthetic.json");
    const INTEGRATIONS: &str = include_str!("../../../fixtures/rest/integrations-synthetic.json");
    const WEBHOOKS: &str = include_str!("../../../fixtures/rest/webhooks-synthetic.json");

    #[test]
    fn authorized_apps_keep_name_scopes_and_time() {
        let list = parse_authorized(AUTHORIZED.as_bytes()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, Snowflake(8001));
        assert_eq!(list[0].name, "Example App");
        assert_eq!(list[0].scopes, ["identify", "guilds"]);
        assert!(
            list[0]
                .authorized_at
                .as_deref()
                .unwrap()
                .starts_with("2026-")
        );
        assert_eq!(list[1].name, "");
        assert_eq!(list[1].authorized_at, None);
        assert_eq!(parse_authorized(b"{}"), None);
    }

    #[test]
    fn integrations_show_type_account_and_bot() {
        let list = parse_integrations(INTEGRATIONS.as_bytes()).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].kind, "discord");
        assert_eq!(list[0].bot.as_deref(), Some("example-bot"));
        assert_eq!(list[1].account, "Example Account");
        assert_eq!(list[1].bot, None);
    }

    #[test]
    fn webhooks_keep_incoming_only_and_the_token_stays_secret() {
        let list = parse_webhooks(WEBHOOKS.as_bytes()).unwrap();
        assert_eq!(list.len(), 2, "the follower webhook is dropped");
        assert_eq!(list[0].channel, Snowflake(10));
        let debug = format!("{list:?}");
        assert!(!debug.contains("fake-token"), "{debug}");
        assert!(debug.contains("REDACTED"));
        assert_eq!(
            list[0].token.as_ref().unwrap().url(list[0].id),
            "https://discord.com/api/webhooks/9001/fake-token-aaaa"
        );
        assert_eq!(list[1].token, None);
        assert_eq!(parse_webhooks(b"{}"), None);
        let many = format!(
            "[{}]",
            vec![r#"{"id":"1","type":1,"channel_id":"2"}"#; MAX_ROWS + 5].join(",")
        );
        assert_eq!(parse_webhooks(many.as_bytes()).unwrap().len(), MAX_ROWS);
    }

    #[test]
    fn bodies_routes_and_client_ids() {
        assert_eq!(
            webhook_edit_body(Some(" Hook "), Some(Snowflake(5)))
                .unwrap()
                .to_string(),
            r#"{"channel_id":"5","name":"Hook"}"#
        );
        assert!(webhook_edit_body(None, None).is_err());
        assert!(webhook_edit_body(Some("  "), None).is_err());
        assert!(name_ok(&"n".repeat(MAX_WEBHOOK_NAME + 1)).is_err());
        assert_eq!(parse_client_id(" 123 "), Some(Snowflake(123)));
        for bad in ["", "12a", "-1", "123456789012345678901"] {
            assert_eq!(parse_client_id(bad), None);
        }
        assert_eq!(
            authorize_url(Snowflake(7)),
            "https://discord.com/oauth2/authorize?client_id=7&scope=bot%20applications.commands"
        );
        for path in [
            "/oauth2/tokens/1".to_owned(),
            "/guilds/1/integrations/2".to_owned(),
            "/webhooks/3".to_owned(),
        ] {
            assert!(write(Method::DELETE, &path).is_ok());
        }
    }
}
