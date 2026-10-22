//! Manual configuration of Discord's own AutoMod (issue #96): list, create,
//! edit, enable/disable and delete a server's rules, with Discord's limits
//! checked before anything is sent. This is Discord's service configured by a
//! person, one request per click, sent once and never retried; there is no
//! client-side moderation engine and no content is watched or scanned here.
//!
//! Keywords, patterns and block messages are private: no `Debug` output prints
//! them. Rules are parsed with every count and length bounded. What this client
//! does not know (trigger metadata keys, action types) is kept verbatim and
//! sent back on an edit, so editing known fields never drops the rest. Wire
//! shapes follow Discord's documentation and are unverified against a live user
//! account (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::fmt;
use std::sync::Arc;

use crate::invite::cut_at;
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

pub const TRIGGER_KEYWORD: u8 = 1;
pub const TRIGGER_SPAM: u8 = 3;
pub const TRIGGER_PRESET: u8 = 4;
pub const TRIGGER_MENTION: u8 = 5;
pub const TRIGGER_PROFILE: u8 = 6;

/// Rules held for one server; Discord allows far fewer.
pub const MAX_RULES: usize = 50;
pub const MAX_RULE_NAME_CHARS: usize = 100;
pub const MAX_KEYWORDS: usize = 1000;
pub const MAX_KEYWORD_CHARS: usize = 60;
pub const MAX_REGEX: usize = 10;
pub const MAX_REGEX_CHARS: usize = 260;
/// Allowed phrases: 100 for a keyword rule, 1000 for a preset rule.
pub const MAX_ALLOW_KEYWORD: usize = 100;
pub const MAX_ALLOW_PRESET: usize = 1000;
pub const MAX_MENTION_LIMIT: u32 = 50;
/// Four weeks, the longest timeout an action may apply.
pub const MAX_TIMEOUT_SECONDS: u32 = 2_419_200;
pub const MAX_BLOCK_MESSAGE_CHARS: usize = 150;
pub const MAX_EXEMPT_ROLES: usize = 20;
pub const MAX_EXEMPT_CHANNELS: usize = 50;
const MAX_ACTIONS: usize = 8;
const MAX_EXTRA_KEYS: usize = 16;
const MAX_NAME_BYTES: usize = 4 * MAX_RULE_NAME_CHARS;
const MAX_ENTRY_BYTES: usize = 4 * MAX_REGEX_CHARS;
const MAX_MESSAGE_BYTES: usize = 4 * MAX_BLOCK_MESSAGE_CHARS;
/// Rule changes kept for the open panel, newest last.
pub const FEED_ITEMS: usize = 8;

/// The keyword presets a preset rule may block.
pub const PRESETS: [(u8, &str); 3] = [(1, "Profanity"), (2, "Sexual content"), (3, "Slurs")];

/// A readable name for a trigger type.
pub fn trigger_name(trigger: u8) -> String {
    match trigger {
        TRIGGER_KEYWORD => "Keywords",
        TRIGGER_SPAM => "Suspected spam",
        TRIGGER_PRESET => "Common word lists",
        TRIGGER_MENTION => "Mention spam",
        TRIGGER_PROFILE => "Member profile",
        other => return format!("Trigger {other}"),
    }
    .to_owned()
}

/// How many rules of a trigger a server may hold, where Discord limits it.
pub fn trigger_limit(trigger: u8) -> Option<usize> {
    match trigger {
        TRIGGER_KEYWORD => Some(6),
        TRIGGER_SPAM | TRIGGER_PRESET | TRIGGER_MENTION | TRIGGER_PROFILE => Some(1),
        _ => None,
    }
}

/// The trigger types the editor creates (member profile rules are listed and
/// can be switched and deleted, but not created here).
pub const CREATABLE: [u8; 4] = [
    TRIGGER_KEYWORD,
    TRIGGER_SPAM,
    TRIGGER_PRESET,
    TRIGGER_MENTION,
];

/// Whether a rule of this trigger carries keywords and patterns.
pub fn has_keywords(trigger: u8) -> bool {
    matches!(trigger, TRIGGER_KEYWORD | TRIGGER_PROFILE)
}

#[derive(Clone, PartialEq, Eq)]
pub enum AutoModAction {
    BlockMessage {
        /// Shown to the author, at most [`MAX_BLOCK_MESSAGE_CHARS`].
        message: Option<String>,
    },
    Alert {
        channel: Snowflake,
    },
    Timeout {
        seconds: u32,
    },
    BlockInteraction,
    /// An action this client does not know, kept as Discord sent it.
    Other(Value),
}

impl fmt::Debug for AutoModAction {
    // Names the kind only: custom messages are the server's private text.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BlockMessage { .. } => "BlockMessage",
            Self::Alert { .. } => "Alert",
            Self::Timeout { .. } => "Timeout",
            Self::BlockInteraction => "BlockInteraction",
            Self::Other(_) => "Other",
        })
    }
}

impl AutoModAction {
    fn wire(&self) -> Value {
        match self {
            Self::BlockMessage { message: None } => json!({ "type": 1 }),
            Self::BlockMessage {
                message: Some(message),
            } => json!({ "type": 1, "metadata": { "custom_message": message } }),
            Self::Alert { channel } => {
                json!({ "type": 2, "metadata": { "channel_id": channel.to_string() } })
            }
            Self::Timeout { seconds } => {
                json!({ "type": 3, "metadata": { "duration_seconds": seconds } })
            }
            Self::BlockInteraction => json!({ "type": 4 }),
            Self::Other(value) => value.clone(),
        }
    }

    fn from_wire(value: Value) -> Self {
        let metadata = &value["metadata"];
        match value["type"].as_u64() {
            Some(1) => Self::BlockMessage {
                message: metadata["custom_message"]
                    .as_str()
                    .filter(|m| !m.is_empty())
                    .map(|m| cut_at(m.to_owned(), MAX_MESSAGE_BYTES)),
            },
            Some(2) => match metadata["channel_id"]
                .as_str()
                .and_then(|id| id.parse::<Snowflake>().ok())
            {
                Some(channel) => Self::Alert { channel },
                None => Self::Other(value),
            },
            Some(3) => match metadata["duration_seconds"]
                .as_u64()
                .and_then(|s| u32::try_from(s).ok())
            {
                Some(seconds) => Self::Timeout { seconds },
                None => Self::Other(value),
            },
            Some(4) => Self::BlockInteraction,
            _ => Self::Other(value),
        }
    }

    /// Whether Discord accepts this action in a rule of `trigger`.
    pub fn allowed_for(&self, trigger: u8) -> bool {
        match self {
            Self::BlockMessage { .. } => !matches!(trigger, TRIGGER_PROFILE),
            Self::Alert { .. } | Self::Other(_) => true,
            Self::Timeout { .. } => matches!(trigger, TRIGGER_KEYWORD | TRIGGER_MENTION),
            Self::BlockInteraction => trigger == TRIGGER_PROFILE,
        }
    }
}

/// One AutoMod rule, as listed and as drafted for a create or an edit.
#[derive(Clone, PartialEq, Eq)]
pub struct AutoModRule {
    pub id: Snowflake,
    pub guild: Snowflake,
    pub name: String,
    pub trigger: u8,
    pub enabled: bool,
    pub keywords: Vec<String>,
    pub regex: Vec<String>,
    pub allow: Vec<String>,
    pub presets: Vec<u8>,
    /// Mentions in one message that trigger a mention spam rule.
    pub mention_limit: u32,
    pub raid_protection: bool,
    pub actions: Vec<AutoModAction>,
    pub exempt_roles: Vec<Snowflake>,
    pub exempt_channels: Vec<Snowflake>,
    /// Trigger metadata keys this client does not know, sent back on edits.
    pub extra: Map<String, Value>,
}

impl fmt::Debug for AutoModRule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AutoModRule")
            .field("id", &self.id)
            .field("trigger", &self.trigger)
            .finish()
    }
}

impl AutoModRule {
    /// An empty rule of one trigger, the start of a create.
    pub fn draft(guild: Snowflake, trigger: u8) -> Self {
        Self {
            id: Snowflake(0),
            guild,
            name: String::new(),
            trigger,
            enabled: true,
            keywords: Vec::new(),
            regex: Vec::new(),
            allow: Vec::new(),
            presets: Vec::new(),
            mention_limit: if trigger == TRIGGER_MENTION { 10 } else { 0 },
            raid_protection: false,
            actions: vec![AutoModAction::BlockMessage { message: None }],
            exempt_roles: Vec::new(),
            exempt_channels: Vec::new(),
            extra: Map::new(),
        }
    }

    /// The text this rule holds, for a byte budget.
    pub fn bytes(&self) -> usize {
        let list = |l: &[String]| l.iter().map(|e| e.len() + 24).sum::<usize>();
        256 + self.name.len()
            + list(&self.keywords)
            + list(&self.regex)
            + list(&self.allow)
            + self.actions.len() * 96
            + (self.exempt_roles.len() + self.exempt_channels.len()) * 8
    }

    fn event_type(&self) -> u8 {
        if self.trigger == TRIGGER_PROFILE {
            2
        } else {
            1
        }
    }

    /// `trigger_metadata` for this rule's trigger, with the unknown keys kept.
    fn metadata(&self) -> Option<Value> {
        let mut meta = self.extra.clone();
        match self.trigger {
            TRIGGER_KEYWORD | TRIGGER_PROFILE => {
                meta.insert("keyword_filter".into(), json!(self.keywords));
                meta.insert("regex_patterns".into(), json!(self.regex));
                meta.insert("allow_list".into(), json!(self.allow));
            }
            TRIGGER_PRESET => {
                meta.insert("presets".into(), json!(self.presets));
                meta.insert("allow_list".into(), json!(self.allow));
            }
            TRIGGER_MENTION => {
                meta.insert("mention_total_limit".into(), json!(self.mention_limit));
                meta.insert(
                    "mention_raid_protection_enabled".into(),
                    json!(self.raid_protection),
                );
            }
            _ => {}
        }
        (!meta.is_empty()).then_some(Value::Object(meta))
    }

    fn actions_wire(&self) -> Value {
        self.actions.iter().map(AutoModAction::wire).collect()
    }
}

/// Why a rule was not sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleError {
    BadName,
    /// Creating a trigger type this client does not create.
    UnsupportedTrigger,
    /// The server already holds as many rules of this trigger as Discord allows.
    TooManyOfKind(u8),
    TooManyKeywords,
    KeywordTooLong,
    TooManyRegex,
    RegexTooLong,
    TooManyAllowed,
    /// A keyword rule needs a keyword or a pattern.
    NoKeywords,
    /// A preset rule needs at least one list, and only the known ones.
    BadPresets,
    BadMentionLimit,
    NoActions,
    /// An action Discord does not accept for this trigger.
    BadAction,
    BadTimeout,
    BlockMessageTooLong,
    TooManyExemptRoles,
    TooManyExemptChannels,
    NothingToChange,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadName => write!(f, "A rule name has 1 to {MAX_RULE_NAME_CHARS} characters."),
            Self::UnsupportedTrigger => f.write_str("This kind of rule is not created here."),
            Self::TooManyOfKind(trigger) => write!(
                f,
                "A server can have at most {} {} rule(s).",
                trigger_limit(*trigger).unwrap_or(0),
                trigger_name(*trigger).to_lowercase()
            ),
            Self::TooManyKeywords => write!(f, "At most {MAX_KEYWORDS} keywords."),
            Self::KeywordTooLong => {
                write!(f, "A keyword has at most {MAX_KEYWORD_CHARS} characters.")
            }
            Self::TooManyRegex => write!(f, "At most {MAX_REGEX} regex patterns."),
            Self::RegexTooLong => write!(f, "A pattern has at most {MAX_REGEX_CHARS} characters."),
            Self::TooManyAllowed => write!(
                f,
                "At most {MAX_ALLOW_KEYWORD} allowed phrases ({MAX_ALLOW_PRESET} for word lists)."
            ),
            Self::NoKeywords => f.write_str("Add at least one keyword or pattern."),
            Self::BadPresets => f.write_str("Choose at least one word list."),
            Self::BadMentionLimit => {
                write!(f, "The mention limit is 1 to {MAX_MENTION_LIMIT}.")
            }
            Self::NoActions => f.write_str("Choose at least one action."),
            Self::BadAction => f.write_str("That action is not available for this kind of rule."),
            Self::BadTimeout => f.write_str("A timeout is 1 second to 4 weeks."),
            Self::BlockMessageTooLong => write!(
                f,
                "The message shown to the author has at most {MAX_BLOCK_MESSAGE_CHARS} characters."
            ),
            Self::TooManyExemptRoles => write!(f, "At most {MAX_EXEMPT_ROLES} exempt roles."),
            Self::TooManyExemptChannels => {
                write!(f, "At most {MAX_EXEMPT_CHANNELS} exempt channels.")
            }
            Self::NothingToChange => f.write_str("Nothing to change."),
        }
    }
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

/// Checks a rule against Discord's limits. `existing` is the server's rule
/// list for a create (the per-trigger limits count against it) and `None` for
/// an edit, where the server already accepted the trigger.
pub fn validate(rule: &AutoModRule, existing: Option<&[AutoModRule]>) -> Result<(), RuleError> {
    let name = chars(rule.name.trim());
    if name == 0 || name > MAX_RULE_NAME_CHARS {
        return Err(RuleError::BadName);
    }
    if let Some(existing) = existing {
        if !CREATABLE.contains(&rule.trigger) {
            return Err(RuleError::UnsupportedTrigger);
        }
        let held = existing
            .iter()
            .filter(|r| r.trigger == rule.trigger)
            .count();
        if trigger_limit(rule.trigger).is_some_and(|limit| held >= limit) {
            return Err(RuleError::TooManyOfKind(rule.trigger));
        }
    }
    match rule.trigger {
        TRIGGER_KEYWORD | TRIGGER_PROFILE => {
            if rule.keywords.len() > MAX_KEYWORDS {
                return Err(RuleError::TooManyKeywords);
            }
            if rule.keywords.iter().any(|k| chars(k) > MAX_KEYWORD_CHARS) {
                return Err(RuleError::KeywordTooLong);
            }
            if rule.regex.len() > MAX_REGEX {
                return Err(RuleError::TooManyRegex);
            }
            if rule.regex.iter().any(|r| chars(r) > MAX_REGEX_CHARS) {
                return Err(RuleError::RegexTooLong);
            }
            if rule.allow.len() > MAX_ALLOW_KEYWORD {
                return Err(RuleError::TooManyAllowed);
            }
            if rule.keywords.is_empty() && rule.regex.is_empty() {
                return Err(RuleError::NoKeywords);
            }
        }
        TRIGGER_PRESET => {
            if rule.presets.is_empty() || rule.presets.iter().any(|p| !(1..=3).contains(p)) {
                return Err(RuleError::BadPresets);
            }
            if rule.allow.len() > MAX_ALLOW_PRESET {
                return Err(RuleError::TooManyAllowed);
            }
        }
        TRIGGER_MENTION if !(1..=MAX_MENTION_LIMIT).contains(&rule.mention_limit) => {
            return Err(RuleError::BadMentionLimit);
        }
        _ => {}
    }
    if rule.allow.iter().any(|a| chars(a) > MAX_KEYWORD_CHARS) {
        return Err(RuleError::KeywordTooLong);
    }
    if rule.actions.is_empty() || rule.actions.len() > MAX_ACTIONS {
        return Err(RuleError::NoActions);
    }
    for action in &rule.actions {
        if !action.allowed_for(rule.trigger) {
            return Err(RuleError::BadAction);
        }
        match action {
            AutoModAction::Timeout { seconds } if !(1..=MAX_TIMEOUT_SECONDS).contains(seconds) => {
                return Err(RuleError::BadTimeout);
            }
            AutoModAction::BlockMessage {
                message: Some(message),
            } if chars(message) > MAX_BLOCK_MESSAGE_CHARS => {
                return Err(RuleError::BlockMessageTooLong);
            }
            _ => {}
        }
    }
    if rule.exempt_roles.len() > MAX_EXEMPT_ROLES {
        return Err(RuleError::TooManyExemptRoles);
    }
    if rule.exempt_channels.len() > MAX_EXEMPT_CHANNELS {
        return Err(RuleError::TooManyExemptChannels);
    }
    Ok(())
}

fn ids(list: &[Snowflake]) -> Value {
    list.iter().map(ToString::to_string).collect()
}

/// The body of a create, after [`validate`].
pub fn create_body(rule: &AutoModRule, existing: &[AutoModRule]) -> Result<Value, RuleError> {
    validate(rule, Some(existing))?;
    let mut body = json!({
        "name": rule.name.trim(),
        "event_type": rule.event_type(),
        "trigger_type": rule.trigger,
        "actions": rule.actions_wire(),
        "enabled": rule.enabled,
        "exempt_roles": ids(&rule.exempt_roles),
        "exempt_channels": ids(&rule.exempt_channels),
    });
    if let Some(meta) = rule.metadata() {
        body["trigger_metadata"] = meta;
    }
    Ok(body)
}

/// The body of an edit: only what differs from `original`, after [`validate`].
/// The trigger type cannot change.
pub fn edit_body(original: &AutoModRule, edited: &AutoModRule) -> Result<Value, RuleError> {
    validate(edited, None)?;
    let mut body = Map::new();
    if edited.name.trim() != original.name {
        body.insert("name".into(), edited.name.trim().into());
    }
    if edited.enabled != original.enabled {
        body.insert("enabled".into(), edited.enabled.into());
    }
    if edited.metadata() != original.metadata()
        && let Some(meta) = edited.metadata()
    {
        body.insert("trigger_metadata".into(), meta);
    }
    if edited.actions != original.actions {
        body.insert("actions".into(), edited.actions_wire());
    }
    if edited.exempt_roles != original.exempt_roles {
        body.insert("exempt_roles".into(), ids(&edited.exempt_roles));
    }
    if edited.exempt_channels != original.exempt_channels {
        body.insert("exempt_channels".into(), ids(&edited.exempt_channels));
    }
    if body.is_empty() {
        return Err(RuleError::NothingToChange);
    }
    Ok(body.into())
}

#[derive(Deserialize)]
struct WireRule {
    id: Snowflake,
    #[serde(default)]
    guild_id: Option<Snowflake>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    trigger_type: u8,
    #[serde(default)]
    trigger_metadata: Option<Map<String, Value>>,
    #[serde(default)]
    actions: Vec<Value>,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    exempt_roles: Vec<Snowflake>,
    #[serde(default)]
    exempt_channels: Vec<Snowflake>,
}

fn strings(meta: &mut Map<String, Value>, key: &str, max: usize, max_bytes: usize) -> Vec<String> {
    match meta.remove(key) {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| cut_at(s.to_owned(), max_bytes)))
            .take(max)
            .collect(),
        _ => Vec::new(),
    }
}

impl WireRule {
    fn into_rule(self, fallback_guild: Snowflake) -> AutoModRule {
        let mut meta = self.trigger_metadata.unwrap_or_default();
        let keywords = strings(&mut meta, "keyword_filter", MAX_KEYWORDS, MAX_ENTRY_BYTES);
        let regex = strings(&mut meta, "regex_patterns", MAX_REGEX, MAX_ENTRY_BYTES);
        let allow = strings(&mut meta, "allow_list", MAX_ALLOW_PRESET, MAX_ENTRY_BYTES);
        let presets = match meta.remove("presets") {
            Some(Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| v.as_u64().and_then(|p| u8::try_from(p).ok()))
                .take(8)
                .collect(),
            _ => Vec::new(),
        };
        let mention_limit = meta
            .remove("mention_total_limit")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        let raid_protection = meta
            .remove("mention_raid_protection_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        meta.retain(|_, _| true);
        let extra: Map<String, Value> = meta.into_iter().take(MAX_EXTRA_KEYS).collect();
        AutoModRule {
            id: self.id,
            guild: self.guild_id.unwrap_or(fallback_guild),
            name: cut_at(self.name, MAX_NAME_BYTES),
            trigger: self.trigger_type,
            enabled: self.enabled,
            keywords,
            regex,
            allow,
            presets,
            mention_limit,
            raid_protection,
            actions: self
                .actions
                .into_iter()
                .take(MAX_ACTIONS)
                .map(AutoModAction::from_wire)
                .collect(),
            exempt_roles: self
                .exempt_roles
                .into_iter()
                .take(MAX_EXEMPT_ROLES)
                .collect(),
            exempt_channels: self
                .exempt_channels
                .into_iter()
                .take(MAX_EXEMPT_CHANNELS)
                .collect(),
            extra,
        }
    }
}

/// Decodes one rule object (a REST answer or a Gateway payload).
pub fn parse_rule(body: &[u8], guild: Snowflake) -> Result<AutoModRule, RestError> {
    let wire: WireRule = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    Ok(wire.into_rule(guild))
}

/// Decodes the rule list, keeping at most [`MAX_RULES`].
pub fn parse_rules(body: &[u8], guild: Snowflake) -> Result<Vec<AutoModRule>, RestError> {
    let wire: Vec<WireRule> = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    Ok(wire
        .into_iter()
        .take(MAX_RULES)
        .map(|rule| rule.into_rule(guild))
        .collect())
}

/// A rule the Gateway reported created, changed or deleted.
#[derive(Clone, PartialEq, Eq)]
pub enum AutoModChange {
    Upsert(Box<AutoModRule>),
    Delete { guild: Snowflake, id: Snowflake },
}

impl AutoModChange {
    pub fn guild(&self) -> Snowflake {
        match self {
            Self::Upsert(rule) => rule.guild,
            Self::Delete { guild, .. } => *guild,
        }
    }
}

impl fmt::Debug for AutoModChange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Upsert(rule) => write!(f, "Upsert({:?})", rule.id),
            Self::Delete { id, .. } => write!(f, "Delete({id:?})"),
        }
    }
}

/// The last few rule changes with a running number, so a panel that is open
/// can apply the ones it has not seen. Bounded by count and by bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoModFeed {
    pub items: Vec<(u64, Arc<AutoModChange>)>,
    seq: u64,
}

const FEED_BYTES: usize = 256 * 1024;

impl AutoModFeed {
    pub fn push(&mut self, change: AutoModChange) {
        self.seq += 1;
        self.items.push((self.seq, Arc::new(change)));
        let cost = |c: &AutoModChange| match c {
            AutoModChange::Upsert(rule) => rule.bytes(),
            AutoModChange::Delete { .. } => 32,
        };
        while self.items.len() > FEED_ITEMS
            || (self.items.len() > 1
                && self.items.iter().map(|(_, c)| cost(c)).sum::<usize>() > FEED_BYTES)
        {
            self.items.remove(0);
        }
    }

    /// The number of the newest change, 0 before any.
    pub fn latest(&self) -> u64 {
        self.seq
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

/// Decodes `AUTO_MODERATION_RULE_CREATE`, `_UPDATE` and `_DELETE`.
pub fn decode_change(raw: &str, deleted: bool) -> Option<AutoModChange> {
    let rule = parse_rule(raw.as_bytes(), Snowflake(0)).ok()?;
    if rule.guild == Snowflake(0) {
        return None;
    }
    Some(if deleted {
        AutoModChange::Delete {
            guild: rule.guild,
            id: rule.id,
        }
    } else {
        AutoModChange::Upsert(Box::new(rule))
    })
}

/// A failed AutoMod request: invalid input (nothing sent) or Discord's answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoModRequestError {
    Invalid(RuleError),
    Rest(RestError),
}

impl From<RestError> for AutoModRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

fn rules_path(guild: Snowflake) -> String {
    format!("/guilds/{guild}/auto-moderation/rules")
}

fn request(method: Method, path: &str) -> Result<RestRequest, RestError> {
    let priority = if method == Method::GET {
        Priority::UserRead
    } else {
        Priority::UserWrite
    };
    Ok(RestRequest::new(Route::new(method, path)?, priority).with_error_detail())
}

impl<C: Clock> RestClient<C> {
    /// The server's rules (`GET /guilds/{g}/auto-moderation/rules`), read when
    /// the user opens the panel.
    pub async fn list_automod_rules(
        &self,
        guild: Snowflake,
    ) -> Result<Vec<AutoModRule>, RestError> {
        let response = self
            .execute(request(Method::GET, &rules_path(guild))?)
            .await?;
        parse_rules(response.body(), guild)
    }

    /// Creates a rule (`POST .../rules`). `None` means Discord accepted it but
    /// its answer could not be read; the rule then shows from the Gateway or a
    /// fresh list.
    pub async fn create_automod_rule(
        &self,
        guild: Snowflake,
        rule: &AutoModRule,
        existing: &[AutoModRule],
    ) -> Result<Option<AutoModRule>, AutoModRequestError> {
        let body = create_body(rule, existing).map_err(AutoModRequestError::Invalid)?;
        let request = request(Method::POST, &rules_path(guild))?.json(&body)?;
        let response = self.execute(request).await?;
        Ok(parse_rule(response.body(), guild).ok())
    }

    /// Changes a rule (`PATCH .../rules/{id}`), sending only what differs.
    pub async fn edit_automod_rule(
        &self,
        guild: Snowflake,
        original: &AutoModRule,
        edited: &AutoModRule,
    ) -> Result<Option<AutoModRule>, AutoModRequestError> {
        let body = edit_body(original, edited).map_err(AutoModRequestError::Invalid)?;
        let path = format!("{}/{}", rules_path(guild), original.id);
        let request = request(Method::PATCH, &path)?.json(&body)?;
        let response = self.execute(request).await?;
        Ok(parse_rule(response.body(), guild).ok())
    }

    /// Deletes a rule (`DELETE .../rules/{id}`).
    pub async fn delete_automod_rule(
        &self,
        guild: Snowflake,
        rule: Snowflake,
    ) -> Result<(), RestError> {
        let path = format!("{}/{rule}", rules_path(guild));
        self.execute(request(Method::DELETE, &path)?)
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULES: &str = include_str!("../../../fixtures/rest/automod-rules.json");
    const G: Snowflake = Snowflake(1_000_000_000_000_000_100);

    fn rules() -> Vec<AutoModRule> {
        parse_rules(RULES.as_bytes(), G).unwrap()
    }

    fn keyword_draft() -> AutoModRule {
        let mut rule = AutoModRule::draft(G, TRIGGER_KEYWORD);
        rule.name = "No links".into();
        rule.keywords = vec!["spam*".into()];
        rule
    }

    #[test]
    fn rules_are_parsed_with_actions_exemptions_and_unknown_parts_kept() {
        let rules = rules();
        assert_eq!(rules.len(), 4);
        let keyword = &rules[0];
        assert_eq!((keyword.trigger, keyword.enabled), (1, true));
        assert_eq!(keyword.keywords, ["bad*", "*worse"]);
        assert_eq!(keyword.regex, ["b+a+d"]);
        assert_eq!(keyword.allow, ["badge"]);
        assert_eq!(
            keyword.actions[0],
            AutoModAction::BlockMessage {
                message: Some("Please keep it friendly.".into())
            }
        );
        assert_eq!(
            keyword.actions[1],
            AutoModAction::Alert {
                channel: Snowflake(1_000_000_000_000_000_300)
            }
        );
        assert_eq!(keyword.actions[2], AutoModAction::Timeout { seconds: 60 });
        assert!(matches!(keyword.actions[3], AutoModAction::Other(_)));
        assert_eq!(keyword.exempt_roles, [Snowflake(1_000_000_000_000_000_400)]);
        assert_eq!(
            keyword.exempt_channels,
            [Snowflake(1_000_000_000_000_000_301)]
        );
        assert_eq!(keyword.extra["future_key"], 7);
        let preset = &rules[1];
        assert_eq!((preset.trigger, preset.presets.clone()), (4, vec![1, 3]));
        assert!(!preset.enabled);
        let mention = &rules[2];
        assert_eq!((mention.mention_limit, mention.raid_protection), (8, true));
        assert_eq!(rules[3].trigger, TRIGGER_PROFILE);
        assert_eq!(rules[3].actions, [AutoModAction::BlockInteraction]);
        assert_eq!(parse_rules(b"{}", G), Err(RestError::InvalidJson));
        assert!(!format!("{keyword:?}").contains("bad"));
    }

    #[test]
    fn a_create_body_carries_what_discord_expects() {
        let mut rule = keyword_draft();
        rule.actions.push(AutoModAction::Alert {
            channel: Snowflake(9),
        });
        rule.exempt_roles = vec![Snowflake(5)];
        let body = create_body(&rule, &[]).unwrap();
        assert_eq!(
            body,
            json!({
                "name": "No links", "event_type": 1, "trigger_type": 1,
                "trigger_metadata": {
                    "keyword_filter": ["spam*"], "regex_patterns": [], "allow_list": []
                },
                "actions": [
                    {"type": 1},
                    {"type": 2, "metadata": {"channel_id": "9"}}
                ],
                "enabled": true, "exempt_roles": ["5"], "exempt_channels": []
            })
        );
        let spam = AutoModRule {
            name: "Spam".into(),
            ..AutoModRule::draft(G, TRIGGER_SPAM)
        };
        assert!(
            create_body(&spam, &[])
                .unwrap()
                .get("trigger_metadata")
                .is_none()
        );
        let mention = AutoModRule {
            name: "Mentions".into(),
            raid_protection: true,
            ..AutoModRule::draft(G, TRIGGER_MENTION)
        };
        assert_eq!(
            create_body(&mention, &[]).unwrap()["trigger_metadata"],
            json!({"mention_total_limit": 10, "mention_raid_protection_enabled": true})
        );
        let preset = AutoModRule {
            name: "Words".into(),
            presets: vec![2],
            ..AutoModRule::draft(G, TRIGGER_PRESET)
        };
        assert_eq!(
            create_body(&preset, &[]).unwrap()["trigger_metadata"],
            json!({"presets": [2], "allow_list": []})
        );
    }

    #[test]
    fn limits_are_checked_as_discord_states_them() {
        let bad = |edit: &dyn Fn(&mut AutoModRule)| {
            let mut rule = keyword_draft();
            edit(&mut rule);
            validate(&rule, Some(&[])).unwrap_err()
        };
        assert_eq!(bad(&|r| r.name = "  ".into()), RuleError::BadName);
        assert_eq!(bad(&|r| r.name = "n".repeat(101)), RuleError::BadName);
        assert_eq!(
            bad(&|r| r.keywords = vec!["k".into(); MAX_KEYWORDS + 1]),
            RuleError::TooManyKeywords
        );
        assert_eq!(
            bad(&|r| r.keywords = vec!["k".repeat(61)]),
            RuleError::KeywordTooLong
        );
        assert_eq!(
            bad(&|r| r.regex = vec!["r".into(); 11]),
            RuleError::TooManyRegex
        );
        assert_eq!(
            bad(&|r| r.regex = vec!["r".repeat(261)]),
            RuleError::RegexTooLong
        );
        assert_eq!(
            bad(&|r| r.allow = vec!["a".into(); 101]),
            RuleError::TooManyAllowed
        );
        assert_eq!(
            bad(&|r| {
                r.keywords.clear();
            }),
            RuleError::NoKeywords
        );
        assert_eq!(bad(&|r| r.actions.clear()), RuleError::NoActions);
        assert_eq!(
            bad(&|r| r.actions = vec![AutoModAction::Timeout { seconds: 2_419_201 }]),
            RuleError::BadTimeout
        );
        assert_eq!(
            bad(&|r| r.actions = vec![AutoModAction::BlockMessage {
                message: Some("m".repeat(151))
            }]),
            RuleError::BlockMessageTooLong
        );
        assert_eq!(
            bad(&|r| r.actions = vec![AutoModAction::BlockInteraction]),
            RuleError::BadAction
        );
        assert_eq!(
            bad(&|r| r.exempt_roles = vec![Snowflake(1); 21]),
            RuleError::TooManyExemptRoles
        );
        assert_eq!(
            bad(&|r| r.exempt_channels = vec![Snowflake(1); 51]),
            RuleError::TooManyExemptChannels
        );
        // Exactly at the limits is accepted.
        let mut edge = keyword_draft();
        edge.keywords = vec!["k".repeat(60); MAX_KEYWORDS];
        edge.regex = vec!["r".repeat(260); MAX_REGEX];
        edge.actions = vec![AutoModAction::Timeout { seconds: 2_419_200 }];
        assert_eq!(validate(&edge, Some(&[])), Ok(()));
        let mut mention = AutoModRule::draft(G, TRIGGER_MENTION);
        mention.name = "m".into();
        mention.mention_limit = 51;
        assert_eq!(validate(&mention, None), Err(RuleError::BadMentionLimit));
        mention.mention_limit = 50;
        assert_eq!(validate(&mention, None), Ok(()));
        let mut preset = AutoModRule::draft(G, TRIGGER_PRESET);
        preset.name = "p".into();
        assert_eq!(validate(&preset, None), Err(RuleError::BadPresets));
        preset.presets = vec![4];
        assert_eq!(validate(&preset, None), Err(RuleError::BadPresets));
    }

    #[test]
    fn per_trigger_counts_and_creatable_kinds_are_enforced_on_create_only() {
        let mut held = Vec::new();
        for n in 0..6 {
            let mut rule = keyword_draft();
            rule.id = Snowflake(n + 1);
            held.push(rule);
        }
        assert_eq!(
            create_body(&keyword_draft(), &held),
            Err(RuleError::TooManyOfKind(TRIGGER_KEYWORD))
        );
        held.pop();
        assert!(create_body(&keyword_draft(), &held).is_ok());
        let mention = AutoModRule {
            name: "m".into(),
            ..AutoModRule::draft(G, TRIGGER_MENTION)
        };
        let existing = [mention.clone()];
        assert_eq!(
            create_body(&mention, &existing),
            Err(RuleError::TooManyOfKind(TRIGGER_MENTION))
        );
        let mut profile = keyword_draft();
        profile.trigger = TRIGGER_PROFILE;
        assert_eq!(
            create_body(&profile, &[]),
            Err(RuleError::UnsupportedTrigger)
        );
        // An edit does not recount: the server already holds the rule.
        assert!(validate(&mention, None).is_ok());
    }

    #[test]
    fn an_edit_sends_only_changes_and_keeps_unknown_metadata_and_actions() {
        let original = rules()[0].clone();
        let mut edited = original.clone();
        assert_eq!(
            edit_body(&original, &edited),
            Err(RuleError::NothingToChange)
        );
        edited.enabled = false;
        assert_eq!(
            edit_body(&original, &edited).unwrap(),
            json!({"enabled": false})
        );
        edited.keywords.push("new".into());
        edited.name = " Renamed ".into();
        let body = edit_body(&original, &edited).unwrap();
        assert_eq!(body["name"], "Renamed");
        assert_eq!(body["trigger_metadata"]["future_key"], 7);
        assert_eq!(
            body["trigger_metadata"]["keyword_filter"],
            json!(["bad*", "*worse", "new"])
        );
        assert!(body.get("actions").is_none());
        // Changing an action resends all of them, the unknown one untouched.
        edited.actions[0] = AutoModAction::BlockMessage { message: None };
        let body = edit_body(&original, &edited).unwrap();
        let actions = body["actions"].as_array().unwrap();
        assert_eq!(actions.len(), 4);
        assert_eq!(actions[3]["type"], 99);
        assert_eq!(actions[3]["metadata"]["future"], true);
        edited.exempt_roles.clear();
        assert_eq!(
            edit_body(&original, &edited).unwrap()["exempt_roles"],
            json!([])
        );
    }

    #[test]
    fn a_parsed_rule_is_bounded() {
        let many: Vec<String> = (0..1_500).map(|n| format!("\"k{n}\"")).collect();
        let long = "x".repeat(5_000);
        let body = format!(
            r#"{{"id":"7","guild_id":"9","name":"{long}","trigger_type":1,
            "trigger_metadata":{{"keyword_filter":[{}, "{long}"]}},"actions":[],"enabled":true}}"#,
            many.join(",")
        );
        let rule = parse_rule(body.as_bytes(), G).unwrap();
        assert_eq!(rule.keywords.len(), MAX_KEYWORDS);
        assert!(rule.name.len() <= MAX_NAME_BYTES);
        let rule = parse_rule(
            format!(
                r#"{{"id":"7","name":"n","trigger_type":1,"trigger_metadata":{{"keyword_filter":["{long}"]}}}}"#
            )
            .as_bytes(),
            G,
        )
        .unwrap();
        assert!(rule.keywords[0].len() <= MAX_ENTRY_BYTES);
        let crowd: Vec<String> = (0..80)
            .map(|n| format!(r#"{{"id":"{}","name":"r","trigger_type":3}}"#, n + 1))
            .collect();
        assert_eq!(
            parse_rules(format!("[{}]", crowd.join(",")).as_bytes(), G)
                .unwrap()
                .len(),
            MAX_RULES
        );
    }

    #[test]
    fn gateway_changes_decode_and_the_feed_is_bounded() {
        let raw = r#"{"id":"7","guild_id":"9","name":"r","trigger_type":3,"enabled":true,"actions":[{"type":1}]}"#;
        assert!(matches!(
            decode_change(raw, false),
            Some(AutoModChange::Upsert(rule)) if rule.id == Snowflake(7) && rule.guild == Snowflake(9)
        ));
        assert_eq!(
            decode_change(raw, true),
            Some(AutoModChange::Delete {
                guild: Snowflake(9),
                id: Snowflake(7)
            })
        );
        assert_eq!(decode_change(r#"{"id":"7"}"#, false), None);
        assert_eq!(decode_change("[]", false), None);
        let mut feed = AutoModFeed::default();
        for n in 0..20 {
            feed.push(AutoModChange::Delete {
                guild: Snowflake(1),
                id: Snowflake(n),
            });
        }
        assert_eq!(feed.items.len(), FEED_ITEMS);
        assert_eq!(feed.latest(), 20);
        assert_eq!(feed.items[0].0, 13);
    }

    #[test]
    fn paths_and_names() {
        assert_eq!(rules_path(Snowflake(5)), "/guilds/5/auto-moderation/rules");
        assert_eq!(trigger_name(5), "Mention spam");
        assert_eq!(trigger_name(77), "Trigger 77");
        assert_eq!(trigger_limit(1), Some(6));
        assert_eq!(trigger_limit(77), None);
    }
}
