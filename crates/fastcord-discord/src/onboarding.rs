//! Joining a server as a member (SPEC §16, issue #91): the rules the server
//! asks a new member to accept (membership screening), the onboarding
//! questions that choose roles and channels, and the read-only server guide.
//! Every function sends one request on the user's click and never retries; a
//! write that fails ambiguously (`RestError::Retryable`) is left to the
//! caller to reconcile. What the account may see or write afterwards comes only
//! from the Gateway (`pending` and the roles of the member, the channels
//! Discord sends), never from an answer given here. All routes are
//! user-client routes unverified against a live account (`docs/PROTOCOL.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::invite::cut_at;
use crate::{Priority, RestClient, RestError, RestRequest, Route};

/// Member flag: the account finished onboarding (bit 1).
pub const COMPLETED_ONBOARDING: u32 = 1 << 1;
/// Member flag: the account started onboarding (bit 3).
pub const STARTED_ONBOARDING: u32 = 1 << 3;

/// Whether onboarding should be offered: not completed, and either started or
/// enabled for the server.
pub fn onboarding_due(flags: u32, enabled: bool) -> bool {
    flags & COMPLETED_ONBOARDING == 0 && (flags & STARTED_ONBOARDING != 0 || enabled)
}

/// Form fields kept from one screening; more make it not acceptable here.
pub const MAX_FIELDS: usize = 10;
/// Rules (values of a TERMS field) shown.
pub const MAX_RULES: usize = 30;
const MAX_RULE_BYTES: usize = 1_000;
/// Prompts, options per prompt and linked ids per option kept from onboarding.
pub const MAX_PROMPTS: usize = 15;
pub const MAX_OPTIONS: usize = 50;
const MAX_LINKED: usize = 50;
/// Guide entries kept per list.
pub const MAX_GUIDE_ITEMS: usize = 20;
const MAX_TITLE_BYTES: usize = 100;
const MAX_TEXT_BYTES: usize = 300;
const MAX_WELCOME_BYTES: usize = 2_000;
/// A body larger than this is not parsed at all.
const MAX_BODY_BYTES: usize = 256 * 1024;

fn title(text: String) -> String {
    cut_at(text, MAX_TITLE_BYTES)
}

fn note(text: Option<String>) -> String {
    cut_at(text.unwrap_or_default(), MAX_TEXT_BYTES)
}

fn parse<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Option<T> {
    if body.len() > MAX_BODY_BYTES {
        return None;
    }
    serde_json::from_slice(body).ok()
}

/// One field of the membership screening form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreeningField {
    /// `TERMS` is the only kind this client answers.
    pub kind: String,
    pub label: String,
    pub rules: Vec<String>,
    pub required: bool,
    /// The field as Discord sent it, sent back with a response.
    raw: Value,
}

impl ScreeningField {
    pub fn is_terms(&self) -> bool {
        self.kind == "TERMS"
    }
}

/// The rules form of a server (`GET /guilds/{id}/member-verification`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screening {
    version: Value,
    /// `enabled`, when Discord sends it with the form.
    pub enabled: Option<bool>,
    pub description: String,
    pub fields: Vec<ScreeningField>,
    /// Discord sent more fields than [`MAX_FIELDS`].
    truncated: bool,
}

impl Screening {
    pub fn from_json(body: &[u8]) -> Option<Self> {
        #[derive(Deserialize)]
        struct Wire {
            version: Value,
            description: Option<String>,
            form_fields: Option<Vec<Value>>,
            enabled: Option<bool>,
        }
        let wire: Wire = parse(body)?;
        if !matches!(wire.version, Value::String(_) | Value::Number(_)) {
            return None;
        }
        let all = wire.form_fields.unwrap_or_default();
        let truncated = all.len() > MAX_FIELDS;
        let fields = all
            .into_iter()
            .take(MAX_FIELDS)
            .filter_map(|raw| {
                let text = |key: &str| raw.get(key).and_then(Value::as_str).unwrap_or_default();
                let kind = cut_at(text("field_type").to_owned(), 32);
                let rules = raw
                    .get("values")
                    .and_then(Value::as_array)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(Value::as_str)
                            .take(MAX_RULES)
                            .map(|rule| cut_at(rule.to_owned(), MAX_RULE_BYTES))
                            .collect()
                    })
                    .unwrap_or_default();
                let label = title(text("label").to_owned());
                let required = raw.get("required").and_then(Value::as_bool) == Some(true);
                (!kind.is_empty()).then_some(ScreeningField {
                    label,
                    required,
                    kind,
                    rules,
                    raw,
                })
            })
            .collect();
        Some(Self {
            version: wire.version,
            enabled: wire.enabled,
            description: note(wire.description),
            fields,
            truncated,
        })
    }

    /// The version to send back with an edit.
    pub(crate) fn version(&self) -> &Value {
        &self.version
    }

    /// Whether every field arrived, so an edit sends them all back.
    pub fn complete(&self) -> bool {
        !self.truncated
    }

    pub(crate) fn raw_fields(&self) -> impl Iterator<Item = &Value> {
        self.fields.iter().map(|field| &field.raw)
    }

    /// The first required field this client cannot answer.
    pub fn unsupported(&self) -> Option<&ScreeningField> {
        self.fields.iter().find(|f| f.required && !f.is_terms())
    }

    /// Whether one click can accept it: a rules field is present, nothing
    /// required is left to the official client, and nothing was cut off.
    pub fn can_accept(&self) -> bool {
        !self.truncated
            && self.unsupported().is_none()
            && self.fields.iter().any(ScreeningField::is_terms)
    }

    /// `PUT /guilds/{id}/requests/@me` body: the version read, every field
    /// back as Discord sent it, with `response: true` on the rules.
    pub fn accept_body(&self) -> Value {
        let fields: Vec<Value> = self
            .fields
            .iter()
            .map(|field| {
                let mut raw = field.raw.clone();
                if field.is_terms()
                    && let Some(object) = raw.as_object_mut()
                {
                    object.insert("response".to_owned(), Value::Bool(true));
                }
                raw
            })
            .collect();
        json!({ "version": self.version, "form_fields": fields })
    }
}

/// What Discord says about the membership application after accepting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplicationStatus {
    Approved,
    /// Submitted for the moderators to review.
    Waiting,
    Rejected,
    /// No status in the answer; the Gateway decides.
    Unknown,
}

impl ApplicationStatus {
    pub fn from_json(body: &[u8]) -> Self {
        #[derive(Deserialize)]
        struct Wire {
            application_status: Option<String>,
        }
        match parse::<Wire>(body)
            .and_then(|wire| wire.application_status)
            .as_deref()
        {
            Some("APPROVED") => Self::Approved,
            Some("SUBMITTED" | "PENDING") => Self::Waiting,
            Some("REJECTED") => Self::Rejected,
            _ => Self::Unknown,
        }
    }
}

/// One answer of an onboarding question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingOption {
    pub id: Snowflake,
    pub title: String,
    pub description: String,
    /// The Unicode emoji, if the option has one (custom emoji are not drawn).
    pub emoji: String,
    pub role_ids: Vec<Snowflake>,
    pub channel_ids: Vec<Snowflake>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingPrompt {
    pub id: Snowflake,
    pub title: String,
    pub single_select: bool,
    pub required: bool,
    /// Asked on joining; the others are only in the settings.
    pub in_onboarding: bool,
    pub options: Vec<OnboardingOption>,
}

/// The onboarding of a server (`GET /guilds/{id}/onboarding`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Onboarding {
    pub enabled: bool,
    pub default_channels: Vec<Snowflake>,
    pub prompts: Vec<OnboardingPrompt>,
}

#[derive(Deserialize)]
struct WireEmoji {
    id: Option<Snowflake>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct WireOption {
    id: Snowflake,
    title: Option<String>,
    description: Option<String>,
    emoji: Option<WireEmoji>,
    role_ids: Option<Vec<Snowflake>>,
    channel_ids: Option<Vec<Snowflake>>,
}

#[derive(Deserialize)]
struct WirePrompt {
    id: Snowflake,
    title: Option<String>,
    #[serde(default)]
    single_select: bool,
    #[serde(default)]
    required: bool,
    #[serde(default = "yes")]
    in_onboarding: bool,
    options: Option<Vec<WireOption>>,
}

fn yes() -> bool {
    true
}

fn linked(ids: Option<Vec<Snowflake>>) -> Vec<Snowflake> {
    let mut ids = ids.unwrap_or_default();
    ids.truncate(MAX_LINKED);
    ids
}

impl Onboarding {
    /// Prompts without an answer to choose are dropped.
    pub fn from_json(body: &[u8]) -> Option<Self> {
        #[derive(Deserialize)]
        struct Wire {
            #[serde(default)]
            enabled: bool,
            default_channel_ids: Option<Vec<Snowflake>>,
            prompts: Option<Vec<WirePrompt>>,
        }
        let wire: Wire = parse(body)?;
        let prompts = wire
            .prompts
            .unwrap_or_default()
            .into_iter()
            .map(|p| OnboardingPrompt {
                id: p.id,
                title: title(p.title.unwrap_or_default()),
                single_select: p.single_select,
                required: p.required,
                in_onboarding: p.in_onboarding,
                options: p
                    .options
                    .unwrap_or_default()
                    .into_iter()
                    .take(MAX_OPTIONS)
                    .map(|o| OnboardingOption {
                        id: o.id,
                        title: title(o.title.unwrap_or_default()),
                        description: note(o.description),
                        emoji: o
                            .emoji
                            .filter(|e| e.id.is_none())
                            .and_then(|e| e.name)
                            .map(|name| cut_at(name, 32))
                            .unwrap_or_default(),
                        role_ids: linked(o.role_ids),
                        channel_ids: linked(o.channel_ids),
                    })
                    .collect(),
            })
            .filter(|prompt| !prompt.options.is_empty())
            .take(MAX_PROMPTS)
            .collect();
        Some(Self {
            enabled: wire.enabled,
            default_channels: linked(wire.default_channel_ids),
            prompts,
        })
    }
}

/// `PUT /guilds/{id}/onboarding-responses` body: the chosen options and when
/// every shown prompt and option was seen (unix milliseconds).
pub fn onboarding_body(prompts: &[OnboardingPrompt], chosen: &[Snowflake], now_ms: u64) -> Value {
    let mut prompts_seen = Map::new();
    let mut options_seen = Map::new();
    for prompt in prompts {
        prompts_seen.insert(prompt.id.to_string(), json!(now_ms));
        for option in &prompt.options {
            options_seen.insert(option.id.to_string(), json!(now_ms));
        }
    }
    json!({
        "onboarding_responses": chosen.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "onboarding_prompts_seen": prompts_seen,
        "onboarding_responses_seen": options_seen,
    })
}

/// A channel the guide points to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideItem {
    pub channel: Snowflake,
    pub title: String,
    pub description: String,
}

/// The server guide (`GET /guilds/{id}/new-member-welcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Guide {
    /// Discord Markdown, shown as a message.
    pub welcome: String,
    pub actions: Vec<GuideItem>,
    pub resources: Vec<GuideItem>,
}

impl Guide {
    pub fn from_json(body: &[u8]) -> Option<Self> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Welcome {
            Text(String),
            Object { message: Option<String> },
        }
        #[derive(Deserialize)]
        struct Item {
            channel_id: Snowflake,
            title: Option<String>,
            description: Option<String>,
        }
        #[derive(Deserialize)]
        struct Wire {
            welcome_message: Option<Welcome>,
            new_member_actions: Option<Vec<Item>>,
            resource_channels: Option<Vec<Item>>,
        }
        let items = |list: Option<Vec<Item>>| -> Vec<GuideItem> {
            list.unwrap_or_default()
                .into_iter()
                .take(MAX_GUIDE_ITEMS)
                .map(|i| GuideItem {
                    channel: i.channel_id,
                    title: title(i.title.unwrap_or_default()),
                    description: note(i.description),
                })
                .collect()
        };
        let wire: Wire = parse(body)?;
        let welcome = match wire.welcome_message {
            Some(Welcome::Text(text)) => text,
            Some(Welcome::Object { message }) => message.unwrap_or_default(),
            None => String::new(),
        };
        Some(Self {
            welcome: cut_at(welcome, MAX_WELCOME_BYTES),
            actions: items(wire.new_member_actions),
            resources: items(wire.resource_channels),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.welcome.is_empty() && self.actions.is_empty() && self.resources.is_empty()
    }
}

fn route(method: Method, guild: Snowflake, rest: &str) -> Result<Route, RestError> {
    Route::new(method, &format!("/guilds/{guild}/{rest}"))
}

impl<C: crate::Clock> RestClient<C> {
    /// The rules form (`GET /guilds/{id}/member-verification?with_guild=false`).
    /// A server without screening answers 404 (`ResourceGone` or `Refused`).
    pub async fn member_verification(&self, guild: Snowflake) -> Result<Screening, RestError> {
        let request = RestRequest::new(
            route(Method::GET, guild, "member-verification?with_guild=false")?,
            Priority::UserRead,
        )
        .with_error_detail();
        let response = self.execute(request).await?;
        Screening::from_json(response.body()).ok_or(RestError::InvalidJson)
    }

    /// Accepts the rules on an explicit click (`PUT /guilds/{id}/requests/@me`).
    /// One request; the member stops being pending only when the Gateway says so.
    pub async fn accept_rules(
        &self,
        guild: Snowflake,
        screening: &Screening,
    ) -> Result<ApplicationStatus, RestError> {
        let request = RestRequest::new(
            route(Method::PUT, guild, "requests/@me")?,
            Priority::UserWrite,
        )
        .with_error_detail()
        .json(&screening.accept_body())?;
        let response = self.execute(request).await?;
        Ok(ApplicationStatus::from_json(response.body()))
    }

    /// The onboarding questions (`GET /guilds/{id}/onboarding`).
    pub async fn onboarding(&self, guild: Snowflake) -> Result<Onboarding, RestError> {
        let request =
            RestRequest::new(route(Method::GET, guild, "onboarding")?, Priority::UserRead)
                .with_error_detail();
        let response = self.execute(request).await?;
        Onboarding::from_json(response.body()).ok_or(RestError::InvalidJson)
    }

    /// Sends the chosen answers on an explicit click
    /// (`PUT /guilds/{id}/onboarding-responses`); never retried.
    pub async fn submit_onboarding(
        &self,
        guild: Snowflake,
        prompts: &[OnboardingPrompt],
        chosen: &[Snowflake],
        now_ms: u64,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(
            route(Method::PUT, guild, "onboarding-responses")?,
            Priority::UserWrite,
        )
        .with_error_detail()
        .json(&onboarding_body(prompts, chosen, now_ms))?;
        self.execute(request).await.map(drop)
    }

    /// The server guide (`GET /guilds/{id}/new-member-welcome`).
    pub async fn new_member_welcome(&self, guild: Snowflake) -> Result<Guide, RestError> {
        let request = RestRequest::new(
            route(Method::GET, guild, "new-member-welcome")?,
            Priority::UserRead,
        )
        .with_error_detail();
        let response = self.execute(request).await?;
        Guide::from_json(response.body()).ok_or(RestError::InvalidJson)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let text = match name {
            "verification" => {
                include_str!("../../../fixtures/rest/member-verification-synthetic.json")
            }
            "onboarding" => include_str!("../../../fixtures/rest/onboarding-synthetic.json"),
            _ => include_str!("../../../fixtures/rest/new-member-welcome-synthetic.json"),
        };
        serde_json::from_str(text).unwrap()
    }

    fn bytes(value: &Value) -> Vec<u8> {
        value.to_string().into_bytes()
    }

    fn path(route: &Route) -> String {
        let query = route
            .url
            .query()
            .map_or_else(String::new, |q| format!("?{q}"));
        route.url.path().to_owned() + &query
    }

    #[test]
    fn rules_parse_with_their_fields_and_answer_only_terms() {
        let terms = Screening::from_json(&bytes(&fixture("verification")["terms"])).unwrap();
        assert_eq!(terms.description, "Welcome to the synthetic server.");
        assert_eq!(terms.fields.len(), 1);
        assert_eq!(terms.fields[0].rules.len(), 3);
        assert!(terms.fields[0].is_terms() && terms.fields[0].required);
        assert!(terms.can_accept());

        let question =
            Screening::from_json(&bytes(&fixture("verification")["with_question"])).unwrap();
        assert_eq!(question.fields.len(), 3);
        assert_eq!(
            question.unsupported().unwrap().label,
            "Why do you want to join?"
        );
        assert!(
            !question.can_accept(),
            "a required question is for the official client"
        );

        let none = Screening::from_json(&bytes(&fixture("verification")["no_rules"])).unwrap();
        assert!(!none.can_accept(), "nothing to accept");
        assert!(Screening::from_json(b"[]").is_none());
        assert!(
            Screening::from_json(br#"{"form_fields":[]}"#).is_none(),
            "no version"
        );
    }

    #[test]
    fn accepting_sends_the_version_and_the_fields_with_a_response_on_the_rules() {
        let sample = fixture("verification");
        let screening = Screening::from_json(&bytes(&sample["terms"])).unwrap();
        let body = screening.accept_body();
        assert_eq!(body["version"], sample["terms"]["version"]);
        let field = &body["form_fields"][0];
        assert_eq!(field["field_type"], "TERMS");
        assert_eq!(field["response"], true);
        assert_eq!(field["values"], sample["terms"]["form_fields"][0]["values"]);

        // An optional question travels back unanswered.
        let mut mixed = sample["with_question"].clone();
        mixed["form_fields"].as_array_mut().unwrap().remove(1);
        let mixed = Screening::from_json(&bytes(&mixed)).unwrap();
        assert!(mixed.can_accept());
        let body = mixed.accept_body();
        assert_eq!(body["form_fields"][0]["response"], true);
        assert!(body["form_fields"][1].get("response").is_none());
    }

    #[test]
    fn screening_is_bounded() {
        let field = json!({"field_type": "TERMS", "label": "x".repeat(500), "required": true,
            "values": vec!["r".repeat(1_500); 40]});
        let one =
            Screening::from_json(&bytes(&json!({"version": "v", "form_fields": [field]}))).unwrap();
        assert_eq!(one.fields[0].rules.len(), MAX_RULES);
        assert!(one.fields[0].rules[0].len() <= MAX_RULE_BYTES);
        assert!(one.fields[0].label.len() <= MAX_TITLE_BYTES);
        assert!(one.can_accept());

        let small = json!({"field_type": "TERMS", "label": "x", "required": true, "values": ["r"]});
        let many = vec![small; MAX_FIELDS + 5];
        let many =
            Screening::from_json(&bytes(&json!({"version": "v", "form_fields": many}))).unwrap();
        assert_eq!(many.fields.len(), MAX_FIELDS);
        assert!(!many.can_accept(), "cut-off fields cannot be answered");
        let huge = vec![b' '; MAX_BODY_BYTES + 1];
        assert!(Screening::from_json(&huge).is_none());
    }

    #[test]
    fn the_application_status_is_read_from_the_answer() {
        let sample = fixture("verification");
        assert_eq!(
            ApplicationStatus::from_json(&bytes(&sample["submitted"])),
            ApplicationStatus::Waiting
        );
        assert_eq!(
            ApplicationStatus::from_json(&bytes(&sample["approved"])),
            ApplicationStatus::Approved
        );
        assert_eq!(
            ApplicationStatus::from_json(br#"{"application_status":"REJECTED"}"#),
            ApplicationStatus::Rejected
        );
        assert_eq!(
            ApplicationStatus::from_json(br#"{"application_status":"PENDING"}"#),
            ApplicationStatus::Waiting
        );
        assert_eq!(
            ApplicationStatus::from_json(b""),
            ApplicationStatus::Unknown
        );
        assert_eq!(
            ApplicationStatus::from_json(b"{}"),
            ApplicationStatus::Unknown
        );
    }

    #[test]
    fn onboarding_prompts_and_options_decode() {
        let parsed = Onboarding::from_json(&bytes(&fixture("onboarding"))).unwrap();
        assert!(parsed.enabled);
        assert_eq!(
            parsed.default_channels,
            vec![Snowflake(201), Snowflake(202)]
        );
        assert_eq!(parsed.prompts.len(), 3);
        let first = &parsed.prompts[0];
        assert!(!first.single_select && first.required && first.in_onboarding);
        assert_eq!(first.options[0].emoji, "\u{265f}\u{fe0f}");
        assert_eq!(first.options[0].role_ids, vec![Snowflake(401)]);
        assert_eq!(first.options[0].channel_ids, vec![Snowflake(203)]);
        assert_eq!(first.options[1].emoji, "", "a custom emoji is not drawn");
        assert_eq!(first.options[1].description, "");
        assert!(parsed.prompts[1].single_select && !parsed.prompts[1].required);
        assert!(!parsed.prompts[2].in_onboarding);
        // A prompt without options, or a body of the wrong shape, gives nothing to choose.
        let empty =
            Onboarding::from_json(br#"{"enabled":true,"prompts":[{"id":"1","options":null}]}"#)
                .unwrap();
        assert!(empty.prompts.is_empty());
        assert!(Onboarding::from_json(b"[]").is_none());
    }

    #[test]
    fn onboarding_is_bounded() {
        let options: Vec<Value> = (1..=MAX_OPTIONS as u64 + 10)
            .map(|id| {
                json!({"id": id.to_string(), "title": "t".repeat(500),
                "role_ids": (1..=200u64).map(|n| n.to_string()).collect::<Vec<_>>()})
            })
            .collect();
        let one = json!({"prompts": [{"id": "1", "options": options}]});
        let parsed = Onboarding::from_json(&bytes(&one)).unwrap();
        assert_eq!(parsed.prompts[0].options.len(), MAX_OPTIONS);
        assert!(parsed.prompts[0].options[0].title.len() <= MAX_TITLE_BYTES);
        assert_eq!(parsed.prompts[0].options[0].role_ids.len(), MAX_LINKED);

        let prompts: Vec<Value> = (1..=MAX_PROMPTS as u64 + 5)
            .map(|id| json!({"id": id.to_string(), "options": [{"id": "9"}]}))
            .collect();
        let parsed = Onboarding::from_json(&bytes(&json!({"prompts": prompts}))).unwrap();
        assert_eq!(parsed.prompts.len(), MAX_PROMPTS);
    }

    #[test]
    fn the_submit_body_lists_choices_and_what_was_seen() {
        let parsed = Onboarding::from_json(&bytes(&fixture("onboarding"))).unwrap();
        let body = onboarding_body(
            &parsed.prompts,
            &[Snowflake(301), Snowflake(312)],
            1_700_000_000_123,
        );
        assert_eq!(body["onboarding_responses"], json!(["301", "312"]));
        assert_eq!(body["onboarding_prompts_seen"]["300"], 1_700_000_000_123u64);
        assert_eq!(
            body["onboarding_prompts_seen"].as_object().unwrap().len(),
            3
        );
        assert_eq!(
            body["onboarding_responses_seen"]["302"],
            1_700_000_000_123u64
        );
        assert_eq!(
            body["onboarding_responses_seen"].as_object().unwrap().len(),
            5
        );
        let none = onboarding_body(&parsed.prompts, &[], 1);
        assert_eq!(none["onboarding_responses"], json!([]));
    }

    #[test]
    fn the_flags_decide_when_onboarding_is_offered() {
        assert!(onboarding_due(STARTED_ONBOARDING, false));
        assert!(onboarding_due(0, true));
        assert!(!onboarding_due(0, false));
        assert!(!onboarding_due(
            STARTED_ONBOARDING | COMPLETED_ONBOARDING,
            true
        ));
        assert!(!onboarding_due(COMPLETED_ONBOARDING, true));
    }

    #[test]
    fn the_guide_keeps_the_welcome_text_and_its_channel_lists() {
        let guide = Guide::from_json(&bytes(&fixture("guide"))).unwrap();
        assert_eq!(guide.welcome, "Welcome, **friend**! Start in <#201>.");
        assert_eq!(guide.actions.len(), 2);
        assert_eq!(guide.actions[0].channel, Snowflake(201));
        assert_eq!(guide.actions[0].title, "Say hello");
        assert_eq!(guide.resources[0].description, "Read first");
        assert!(!guide.is_empty());
        let plain =
            Guide::from_json(br#"{"welcome_message":"Hi","new_member_actions":null}"#).unwrap();
        assert_eq!(plain.welcome, "Hi");
        assert!(Guide::from_json(b"{}").unwrap().is_empty());
        let long = json!({"welcome_message": {"message": "w".repeat(10_000)},
            "resource_channels": (1..=40u64).map(|n| json!({"channel_id": n.to_string()})).collect::<Vec<_>>()});
        let long = Guide::from_json(&bytes(&long)).unwrap();
        assert_eq!(long.welcome.len(), MAX_WELCOME_BYTES);
        assert_eq!(long.resources.len(), MAX_GUIDE_ITEMS);
    }

    #[test]
    fn requests_are_one_route_each() {
        let guild = Snowflake(100);
        let verify = route(Method::GET, guild, "member-verification?with_guild=false").unwrap();
        assert_eq!(
            path(&verify),
            "/api/v10/guilds/100/member-verification?with_guild=false"
        );
        let accept = route(Method::PUT, guild, "requests/@me").unwrap();
        assert_eq!(path(&accept), "/api/v10/guilds/100/requests/@me");
        assert_eq!(accept.key().method(), &Method::PUT);
        let answer = route(Method::PUT, guild, "onboarding-responses").unwrap();
        assert_eq!(path(&answer), "/api/v10/guilds/100/onboarding-responses");
        assert_eq!(
            route(Method::GET, guild, "onboarding")
                .unwrap()
                .key()
                .method(),
            &Method::GET
        );
    }
}
