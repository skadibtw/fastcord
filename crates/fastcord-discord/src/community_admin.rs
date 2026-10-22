//! Configuring how a server welcomes members (SPEC §16, issue #97): the rules
//! screening, the onboarding questions with default channels, the server
//! guide, and turning Community on or off. Each editor loads what Discord
//! holds into a document that keeps every field this client does not edit,
//! changes only what the user changed, and sends the whole object once on the
//! user's click (never retried). Nothing is applied locally: the answer, or
//! the Gateway's `GUILD_UPDATE` for Community, is what the caller shows next.
//! Local checks only catch references the account cannot make (a role it may
//! not assign, a channel it cannot see); counts and sizes are bounded, and
//! every other refusal keeps Discord's own words. All routes and shapes are
//! unverified against a live account (`docs/PROTOCOL.md`).

use std::fmt;

use fastcord_model::Snowflake;
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::invite::cut_at;
use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route, Screening};

/// Rules a screening may list, and the characters of one rule.
pub const MAX_RULES_EDITED: usize = 16;
pub const MAX_RULE_CHARS: usize = 300;
pub const MAX_RULES_DESCRIPTION_CHARS: usize = 300;
pub const MAX_PROMPTS_EDITED: usize = 15;
pub const MAX_OPTIONS_EDITED: usize = 50;
pub const MAX_PROMPT_TITLE_CHARS: usize = 100;
pub const MAX_OPTION_TITLE_CHARS: usize = 50;
pub const MAX_OPTION_DESCRIPTION_CHARS: usize = 100;
pub const MAX_NEW_MEMBER_ACTIONS: usize = 5;
pub const MAX_RESOURCE_CHANNELS: usize = 7;
pub const MAX_ENTRY_TITLE_CHARS: usize = 50;
pub const MAX_ENTRY_DESCRIPTION_CHARS: usize = 100;
pub const MAX_WELCOME_CHARS: usize = 300;
/// The most a document keeps of one list read from Discord; more makes the
/// document read-only rather than losing the rest on save.
const MAX_LINKED: usize = 50;
const MAX_LIST: usize = 20;
const MAX_BODY_BYTES: usize = 256 * 1024;
const MAX_EMOJI_BYTES: usize = 32;
const MAX_TEXT_BYTES: usize = 2_000;
/// Discord's epoch (2015-01-01) in Unix milliseconds, for generated ids.
const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;

/// Onboarding modes: default channels only, or channels and questions.
pub const ONBOARDING_MODES: [(u8, &str); 2] = [(0, "Default (channels only)"), (1, "Advanced")];
/// Verification levels Community needs (at least Low).
pub const VERIFICATION_LEVELS: [(u8, &str); 4] =
    [(1, "Low"), (2, "Medium"), (3, "High"), (4, "Highest")];

/// Why a document is not sent. Nothing was sent when one of these is shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommunityError {
    /// The document did not arrive whole (too many entries), so saving it
    /// would delete the rest.
    NotEditable,
    TooManyRules,
    RuleTooLong,
    DescriptionTooLong,
    TooManyPrompts,
    TooManyOptions,
    EmptyTitle,
    TitleTooLong,
    EmptyPrompt,
    BadEmoji,
    UnknownRole(Snowflake),
    /// A role the account may not give (above its own, managed, `@everyone`).
    UnassignableRole(Snowflake),
    UnknownChannel(Snowflake),
    /// A default channel `@everyone` cannot view.
    NotEveryoneVisible(Snowflake),
    TooManyActions,
    TooManyResources,
    WelcomeTooLong,
    NeedsChannels,
    NeedsAuthor,
}

impl fmt::Display for CommunityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotEditable => f.write_str(
                "Discord sent more entries than this client keeps, so saving would delete the rest. Edit this in the official client.",
            ),
            Self::TooManyRules => write!(f, "A server has at most {MAX_RULES_EDITED} rules."),
            Self::RuleTooLong => write!(f, "A rule has at most {MAX_RULE_CHARS} characters."),
            Self::DescriptionTooLong => write!(f, "That description is too long."),
            Self::TooManyPrompts => write!(f, "At most {MAX_PROMPTS_EDITED} questions."),
            Self::TooManyOptions => write!(f, "At most {MAX_OPTIONS_EDITED} answers per question."),
            Self::EmptyTitle => f.write_str("Every question, answer and entry needs a title."),
            Self::TitleTooLong => f.write_str("A title or description is too long."),
            Self::EmptyPrompt => f.write_str("Every question needs at least one answer."),
            Self::BadEmoji => f.write_str("Use a Unicode emoji (custom emoji are kept, not added)."),
            Self::UnknownRole(_) => f.write_str("A chosen role no longer exists."),
            Self::UnassignableRole(_) => f.write_str(
                "A chosen role is one you cannot give: it is above your highest role, managed, or @everyone.",
            ),
            Self::UnknownChannel(_) => f.write_str("A chosen channel is not one you can see."),
            Self::NotEveryoneVisible(_) => {
                f.write_str("Default channels must be text channels @everyone can view.")
            }
            Self::TooManyActions => write!(f, "At most {MAX_NEW_MEMBER_ACTIONS} new-member actions."),
            Self::TooManyResources => write!(f, "At most {MAX_RESOURCE_CHANNELS} resource channels."),
            Self::WelcomeTooLong => {
                write!(f, "The welcome message has at most {MAX_WELCOME_CHARS} characters.")
            }
            Self::NeedsChannels => {
                f.write_str("Community needs a rules channel and a public updates channel.")
            }
            Self::NeedsAuthor => f.write_str("The welcome message needs an author."),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommunityRequestError {
    Invalid(CommunityError),
    Rest(RestError),
}

impl From<RestError> for CommunityRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

/// What the account may reference, from the reducer's read model. An id that
/// the saved document already holds is always kept: the user may remove it
/// but it is never rejected for being out of reach.
#[derive(Clone, Copy, Debug)]
pub struct Known<'a> {
    /// Channels the account can see.
    pub channels: &'a [Snowflake],
    /// Every role of the server.
    pub roles: &'a [Snowflake],
    /// Roles the account may give (`assign_lock` is none).
    pub assignable: &'a [Snowflake],
    /// Text channels `@everyone` can view.
    pub default_ok: &'a [Snowflake],
}

fn chars(text: &str) -> usize {
    text.chars().count()
}

fn ids(list: &[Snowflake]) -> Value {
    list.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .into()
}

fn id_list(value: Option<&Value>) -> Option<Vec<Snowflake>> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Some(Vec::new());
    };
    let list = value.as_array()?;
    if list.len() > MAX_LINKED {
        return None;
    }
    list.iter()
        .map(|v| serde_json::from_value(v.clone()).ok())
        .collect()
}

fn text(map: &Map<String, Value>, key: &str, limit: usize) -> String {
    cut_at(
        map.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        limit,
    )
}

/// A client-generated, snowflake-shaped id for a new prompt, option or entry:
/// milliseconds since the Discord epoch above 22 bits of a sequence number.
/// This is the shape the official client is believed to send; unverified.
pub fn fresh_id(now_ms: u64, sequence: u64) -> Snowflake {
    Snowflake((now_ms.saturating_sub(DISCORD_EPOCH_MS) << 22) | (sequence & 0x3F_FFFF))
}

/// A Unicode emoji only: custom emoji (with an id) are kept as they are but
/// never added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Emoji {
    None,
    Unicode(String),
    Custom {
        id: Snowflake,
        name: String,
        animated: bool,
    },
}

impl Emoji {
    fn read(map: &Map<String, Value>) -> Self {
        let nested = map.get("emoji").and_then(Value::as_object);
        let pick = |nested_key: &str, flat: &str| {
            nested
                .and_then(|n| n.get(nested_key))
                .filter(|v| !v.is_null())
                .or_else(|| map.get(flat).filter(|v| !v.is_null()))
        };
        let name = pick("name", "emoji_name")
            .and_then(Value::as_str)
            .map(|n| cut_at(n.to_owned(), MAX_EMOJI_BYTES))
            .unwrap_or_default();
        if let Some(id) =
            pick("id", "emoji_id").and_then(|v| serde_json::from_value(v.clone()).ok())
        {
            let animated =
                pick("animated", "emoji_animated").and_then(Value::as_bool) == Some(true);
            return Self::Custom { id, name, animated };
        }
        if name.is_empty() {
            Self::None
        } else {
            Self::Unicode(name)
        }
    }

    /// The text field shown for it; empty for none or a custom emoji.
    pub fn text(&self) -> &str {
        match self {
            Self::Unicode(text) => text,
            _ => "",
        }
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, Self::Custom { .. })
    }

    /// What typing in the emoji field makes of it; a custom emoji stays until
    /// something is typed.
    pub fn typed(&self, input: &str) -> Self {
        let input = input.trim();
        match (self, input.is_empty()) {
            (Self::Custom { .. }, true) => self.clone(),
            (_, true) => Self::None,
            _ => Self::Unicode(cut_at(input.to_owned(), MAX_EMOJI_BYTES)),
        }
    }

    fn check(&self) -> Result<(), CommunityError> {
        match self {
            Self::Unicode(text)
                if text.is_ascii()
                    || text.contains(':')
                    || text.chars().any(char::is_whitespace) =>
            {
                Err(CommunityError::BadEmoji)
            }
            _ => Ok(()),
        }
    }

    /// The flat fields (`emoji_id`, `emoji_name`, `emoji_animated`) and the
    /// nested `emoji` object, written into an option or entry.
    fn write(&self, out: &mut Map<String, Value>, nested: bool) {
        let (id, name, animated) = match self {
            Self::None => (Value::Null, Value::Null, false),
            Self::Unicode(text) => (Value::Null, text.clone().into(), false),
            Self::Custom { id, name, animated } => {
                (id.to_string().into(), name.clone().into(), *animated)
            }
        };
        if nested {
            let emoji = if matches!(self, Self::None) {
                Value::Null
            } else {
                json!({ "id": id, "name": name, "animated": animated })
            };
            out.insert("emoji".into(), emoji);
        }
        out.insert("emoji_id".into(), id);
        out.insert("emoji_name".into(), name);
        out.insert("emoji_animated".into(), animated.into());
    }
}

const EMOJI_KEYS: [&str; 4] = ["emoji", "emoji_id", "emoji_name", "emoji_animated"];

/// What the reducer keeps of every key except `own`, so an edit sends the
/// rest back unchanged.
fn rest_of(map: &Map<String, Value>, own: &[&str]) -> Map<String, Value> {
    map.iter()
        .filter(|(key, _)| !own.contains(&key.as_str()) && !EMOJI_KEYS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn titled(title: &str, limit: usize, required: bool) -> Result<String, CommunityError> {
    let title = title.trim();
    if required && title.is_empty() {
        return Err(CommunityError::EmptyTitle);
    }
    if chars(title) > limit {
        return Err(CommunityError::TitleTooLong);
    }
    Ok(title.to_owned())
}

fn allowed(id: Snowflake, ok: &[Snowflake], baseline: &[Snowflake]) -> bool {
    ok.contains(&id) || baseline.contains(&id)
}

// ---------------------------------------------------------------- rules

/// What the rules editor changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RulesEdit {
    pub enabled: bool,
    pub description: String,
    /// The `TERMS` field values; blank lines are dropped.
    pub rules: Vec<String>,
}

impl RulesEdit {
    /// The editor's starting point for a screening, or for a server that has
    /// none yet.
    pub fn from_screening(screening: Option<&Screening>) -> Self {
        let Some(screening) = screening else {
            return Self {
                enabled: false,
                description: String::new(),
                rules: Vec::new(),
            };
        };
        Self {
            // A form Discord returns is one members are asked to accept
            // unless it says otherwise.
            enabled: screening.enabled.unwrap_or(true),
            description: screening.description.clone(),
            rules: screening
                .fields
                .iter()
                .find(|f| f.is_terms())
                .map(|f| f.rules.clone())
                .unwrap_or_default(),
        }
    }

    /// Whether Discord has other field kinds, which are only listed.
    pub fn other_fields(screening: &Screening) -> Vec<(String, String)> {
        screening
            .fields
            .iter()
            .filter(|f| !f.is_terms())
            .map(|f| (f.kind.clone(), f.label.clone()))
            .collect()
    }
}

/// `PATCH /guilds/{id}/member-verification` body: the version read (when
/// there is one), every field back as Discord sent it with the rules replaced,
/// the description and the switch.
pub fn screening_body(
    current: Option<&Screening>,
    edit: &RulesEdit,
) -> Result<Value, CommunityError> {
    if current.is_some_and(|s| !s.complete()) {
        return Err(CommunityError::NotEditable);
    }
    let rules: Vec<&str> = edit
        .rules
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    if rules.len() > MAX_RULES_EDITED {
        return Err(CommunityError::TooManyRules);
    }
    if rules.iter().any(|r| chars(r) > MAX_RULE_CHARS) {
        return Err(CommunityError::RuleTooLong);
    }
    let description = edit.description.trim();
    if chars(description) > MAX_RULES_DESCRIPTION_CHARS {
        return Err(CommunityError::DescriptionTooLong);
    }
    let values: Value = rules
        .iter()
        .map(|r| Value::from(*r))
        .collect::<Vec<_>>()
        .into();
    let mut fields: Vec<Value> = Vec::new();
    let mut placed = false;
    for raw in current.into_iter().flat_map(Screening::raw_fields) {
        if raw.get("field_type").and_then(Value::as_str) != Some("TERMS") {
            fields.push(raw.clone());
        } else if !placed && !rules.is_empty() {
            placed = true;
            let mut field = raw.clone();
            field["values"] = values.clone();
            fields.push(field);
        }
    }
    if !placed && !rules.is_empty() {
        fields.push(json!({
            "field_type": "TERMS",
            "label": "Read and agree to the server rules",
            "required": true,
            "values": values,
        }));
    }
    let mut body = json!({
        "form_fields": fields,
        "description": if description.is_empty() { Value::Null } else { description.into() },
        "enabled": edit.enabled,
    });
    if let Some(screening) = current {
        body["version"] = screening.version().clone();
    }
    Ok(body)
}

// ----------------------------------------------------------- onboarding

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionDoc {
    raw: Map<String, Value>,
    pub id: Snowflake,
    pub title: String,
    pub description: String,
    pub emoji: Emoji,
    pub role_ids: Vec<Snowflake>,
    pub channel_ids: Vec<Snowflake>,
}

impl OptionDoc {
    pub fn new(id: Snowflake) -> Self {
        Self {
            raw: Map::new(),
            id,
            title: String::new(),
            description: String::new(),
            emoji: Emoji::None,
            role_ids: Vec::new(),
            channel_ids: Vec::new(),
        }
    }

    fn read(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        Some(Self {
            id: serde_json::from_value(map.get("id")?.clone()).ok()?,
            title: text(map, "title", MAX_TEXT_BYTES),
            description: text(map, "description", MAX_TEXT_BYTES),
            emoji: Emoji::read(map),
            role_ids: id_list(map.get("role_ids"))?,
            channel_ids: id_list(map.get("channel_ids"))?,
            raw: rest_of(
                map,
                &["id", "title", "description", "role_ids", "channel_ids"],
            ),
        })
    }

    fn body(&self) -> Result<Value, CommunityError> {
        self.emoji.check()?;
        let description = titled(&self.description, MAX_OPTION_DESCRIPTION_CHARS, false)?;
        let mut map = self.raw.clone();
        map.insert("id".into(), self.id.to_string().into());
        map.insert(
            "title".into(),
            titled(&self.title, MAX_OPTION_TITLE_CHARS, true)?.into(),
        );
        map.insert(
            "description".into(),
            if description.is_empty() {
                Value::Null
            } else {
                description.into()
            },
        );
        map.insert("role_ids".into(), ids(&self.role_ids));
        map.insert("channel_ids".into(), ids(&self.channel_ids));
        self.emoji.write(&mut map, true);
        Ok(map.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptDoc {
    raw: Map<String, Value>,
    pub id: Snowflake,
    pub title: String,
    pub single_select: bool,
    pub required: bool,
    pub in_onboarding: bool,
    pub options: Vec<OptionDoc>,
}

impl PromptDoc {
    pub fn new(id: Snowflake) -> Self {
        let mut raw = Map::new();
        // Multiple choice (the other kind is a dropdown); unverified.
        raw.insert("type".into(), 0.into());
        Self {
            raw,
            id,
            title: String::new(),
            single_select: false,
            required: false,
            in_onboarding: true,
            options: Vec::new(),
        }
    }

    fn read(value: &Value) -> Option<(Self, bool)> {
        let map = value.as_object()?;
        let options = match map.get("options") {
            None | Some(Value::Null) => &[][..],
            Some(options) => options.as_array()?.as_slice(),
        };
        let truncated = options.len() > MAX_OPTIONS_EDITED;
        Some((
            Self {
                id: serde_json::from_value(map.get("id")?.clone()).ok()?,
                title: text(map, "title", MAX_TEXT_BYTES),
                single_select: map.get("single_select").and_then(Value::as_bool) == Some(true),
                required: map.get("required").and_then(Value::as_bool) == Some(true),
                in_onboarding: map.get("in_onboarding").and_then(Value::as_bool) != Some(false),
                options: options
                    .iter()
                    .take(MAX_OPTIONS_EDITED)
                    .map(OptionDoc::read)
                    .collect::<Option<_>>()?,
                raw: rest_of(
                    map,
                    &[
                        "id",
                        "title",
                        "single_select",
                        "required",
                        "in_onboarding",
                        "options",
                    ],
                ),
            },
            truncated,
        ))
    }
}

/// The onboarding of a server as the editor holds it
/// (`GET /guilds/{id}/onboarding`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OnboardingDoc {
    raw: Map<String, Value>,
    truncated: bool,
    pub enabled: bool,
    /// 0 default channels only, 1 channels and questions.
    pub mode: u8,
    pub default_channels: Vec<Snowflake>,
    pub prompts: Vec<PromptDoc>,
}

impl OnboardingDoc {
    pub fn from_json(body: &[u8]) -> Option<Self> {
        if body.len() > MAX_BODY_BYTES {
            return None;
        }
        let value: Value = serde_json::from_slice(body).ok()?;
        let map = value.as_object()?;
        let prompts = match map.get("prompts") {
            None | Some(Value::Null) => &[][..],
            Some(prompts) => prompts.as_array()?.as_slice(),
        };
        let mut truncated = prompts.len() > MAX_PROMPTS_EDITED;
        let mut read = Vec::new();
        for prompt in prompts.iter().take(MAX_PROMPTS_EDITED) {
            let (prompt, cut) = PromptDoc::read(prompt)?;
            truncated |= cut;
            read.push(prompt);
        }
        Some(Self {
            truncated,
            enabled: map.get("enabled").and_then(Value::as_bool) == Some(true),
            mode: map
                .get("mode")
                .and_then(Value::as_u64)
                .and_then(|m| u8::try_from(m).ok())
                .unwrap_or(0),
            default_channels: id_list(map.get("default_channel_ids"))?,
            prompts: read,
            raw: rest_of(map, &["enabled", "mode", "default_channel_ids", "prompts"]),
        })
    }

    /// Whether every prompt and answer arrived, so a save loses nothing.
    pub fn complete(&self) -> bool {
        !self.truncated
    }

    fn linked<'a>(&'a self, pick: impl Fn(&'a OptionDoc) -> &'a [Snowflake]) -> Vec<Snowflake> {
        self.prompts
            .iter()
            .flat_map(|p| &p.options)
            .flat_map(pick)
            .copied()
            .collect()
    }

    /// `PUT /guilds/{id}/onboarding` body: the whole object, checked against
    /// what the account may reference. `baseline` is the saved document.
    pub fn body(&self, baseline: &Self, known: &Known<'_>) -> Result<Value, CommunityError> {
        if self.truncated || baseline.truncated {
            return Err(CommunityError::NotEditable);
        }
        if self.prompts.len() > MAX_PROMPTS_EDITED {
            return Err(CommunityError::TooManyPrompts);
        }
        let (old_roles, old_channels) = (
            baseline.linked(|o| &o.role_ids),
            baseline.linked(|o| &o.channel_ids),
        );
        for channel in &self.default_channels {
            if !allowed(*channel, known.default_ok, &baseline.default_channels) {
                return Err(if known.channels.contains(channel) {
                    CommunityError::NotEveryoneVisible(*channel)
                } else {
                    CommunityError::UnknownChannel(*channel)
                });
            }
        }
        let mut prompts = Vec::new();
        for prompt in &self.prompts {
            if prompt.options.len() > MAX_OPTIONS_EDITED {
                return Err(CommunityError::TooManyOptions);
            }
            if prompt.options.is_empty() {
                return Err(CommunityError::EmptyPrompt);
            }
            for option in &prompt.options {
                for role in &option.role_ids {
                    if !known.roles.contains(role) {
                        return Err(CommunityError::UnknownRole(*role));
                    }
                    if !allowed(*role, known.assignable, &old_roles) {
                        return Err(CommunityError::UnassignableRole(*role));
                    }
                }
                for channel in &option.channel_ids {
                    if !allowed(*channel, known.channels, &old_channels) {
                        return Err(CommunityError::UnknownChannel(*channel));
                    }
                }
            }
            let mut map = prompt.raw.clone();
            map.insert("id".into(), prompt.id.to_string().into());
            map.insert(
                "title".into(),
                titled(&prompt.title, MAX_PROMPT_TITLE_CHARS, true)?.into(),
            );
            map.insert("single_select".into(), prompt.single_select.into());
            map.insert("required".into(), prompt.required.into());
            map.insert("in_onboarding".into(), prompt.in_onboarding.into());
            map.insert(
                "options".into(),
                prompt
                    .options
                    .iter()
                    .map(OptionDoc::body)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            );
            prompts.push(Value::from(map));
        }
        let mut body = self.raw.clone();
        body.insert("enabled".into(), self.enabled.into());
        body.insert("mode".into(), self.mode.into());
        body.insert("default_channel_ids".into(), ids(&self.default_channels));
        body.insert("prompts".into(), prompts.into());
        Ok(body.into())
    }
}

// ---------------------------------------------------------------- guide

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideEntry {
    raw: Map<String, Value>,
    pub channel: Snowflake,
    pub title: String,
    pub description: String,
    pub emoji: Emoji,
}

impl GuideEntry {
    pub fn new(channel: Snowflake) -> Self {
        Self {
            raw: Map::new(),
            channel,
            title: String::new(),
            description: String::new(),
            emoji: Emoji::None,
        }
    }

    fn read(value: &Value) -> Option<Self> {
        let map = value.as_object()?;
        Some(Self {
            channel: serde_json::from_value(map.get("channel_id")?.clone()).ok()?,
            title: text(map, "title", MAX_TEXT_BYTES),
            description: text(map, "description", MAX_TEXT_BYTES),
            emoji: Emoji::read(map),
            raw: rest_of(map, &["channel_id", "title", "description"]),
        })
    }

    fn body(&self, channels: &[Snowflake], old: &[Snowflake]) -> Result<Value, CommunityError> {
        if !allowed(self.channel, channels, old) {
            return Err(CommunityError::UnknownChannel(self.channel));
        }
        self.emoji.check()?;
        let mut map = self.raw.clone();
        map.insert("channel_id".into(), self.channel.to_string().into());
        map.insert(
            "title".into(),
            titled(&self.title, MAX_ENTRY_TITLE_CHARS, true)?.into(),
        );
        map.insert(
            "description".into(),
            titled(&self.description, MAX_ENTRY_DESCRIPTION_CHARS, false)?.into(),
        );
        self.emoji.write(&mut map, false);
        Ok(map.into())
    }
}

/// The server guide as the editor holds it
/// (`GET /guilds/{id}/new-member-welcome`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuideDoc {
    raw: Map<String, Value>,
    truncated: bool,
    /// The welcome message's text, Discord Markdown.
    pub welcome: String,
    /// The people shown as its author; kept as they are.
    pub authors: Vec<Snowflake>,
    pub actions: Vec<GuideEntry>,
    pub resources: Vec<GuideEntry>,
}

impl GuideDoc {
    /// A server without a guide yet.
    pub fn empty() -> Self {
        Self {
            raw: Map::new(),
            truncated: false,
            welcome: String::new(),
            authors: Vec::new(),
            actions: Vec::new(),
            resources: Vec::new(),
        }
    }

    pub fn from_json(body: &[u8]) -> Option<Self> {
        if body.len() > MAX_BODY_BYTES {
            return None;
        }
        let value: Value = serde_json::from_slice(body).ok()?;
        let map = value.as_object()?;
        let entries = |key: &str| -> Option<(Vec<GuideEntry>, bool)> {
            let list = match map.get(key) {
                None | Some(Value::Null) => &[][..],
                Some(list) => list.as_array()?.as_slice(),
            };
            let entries = list
                .iter()
                .take(MAX_LIST)
                .map(GuideEntry::read)
                .collect::<Option<_>>()?;
            Some((entries, list.len() > MAX_LIST))
        };
        let (actions, cut_actions) = entries("new_member_actions")?;
        let (resources, cut_resources) = entries("resource_channels")?;
        let (welcome, authors) = match map.get("welcome_message") {
            Some(Value::String(text)) => (text.clone(), Vec::new()),
            Some(Value::Object(welcome)) => (
                text(welcome, "message", MAX_TEXT_BYTES),
                id_list(welcome.get("author_ids"))?,
            ),
            _ => (String::new(), Vec::new()),
        };
        Some(Self {
            truncated: cut_actions || cut_resources,
            welcome,
            authors,
            actions,
            resources,
            raw: rest_of(
                map,
                &["welcome_message", "new_member_actions", "resource_channels"],
            ),
        })
    }

    pub fn complete(&self) -> bool {
        !self.truncated
    }

    /// `PUT /guilds/{id}/new-member-welcome` body: the whole object. `me`
    /// becomes the author of a message that has none.
    pub fn body(
        &self,
        baseline: &Self,
        channels: &[Snowflake],
        me: Snowflake,
    ) -> Result<Value, CommunityError> {
        if self.truncated || baseline.truncated {
            return Err(CommunityError::NotEditable);
        }
        // A list the server already has beyond the limit may stay as it is.
        if self.actions.len() > MAX_NEW_MEMBER_ACTIONS.max(baseline.actions.len()) {
            return Err(CommunityError::TooManyActions);
        }
        if self.resources.len() > MAX_RESOURCE_CHANNELS.max(baseline.resources.len()) {
            return Err(CommunityError::TooManyResources);
        }
        let welcome = self.welcome.trim();
        if chars(welcome) > MAX_WELCOME_CHARS {
            return Err(CommunityError::WelcomeTooLong);
        }
        let entries = |list: &[GuideEntry], old: &[GuideEntry]| -> Result<Value, CommunityError> {
            let old: Vec<Snowflake> = old.iter().map(|e| e.channel).collect();
            Ok(list
                .iter()
                .map(|e| e.body(channels, &old))
                .collect::<Result<Vec<_>, _>>()?
                .into())
        };
        let authors = if self.authors.is_empty() {
            vec![me]
        } else {
            self.authors.clone()
        };
        let mut body = self.raw.clone();
        body.insert(
            "welcome_message".into(),
            json!({ "author_ids": ids(&authors), "message": welcome }),
        );
        body.insert(
            "new_member_actions".into(),
            entries(&self.actions, &baseline.actions)?,
        );
        body.insert(
            "resource_channels".into(),
            entries(&self.resources, &baseline.resources)?,
        );
        Ok(body.into())
    }
}

// ------------------------------------------------------------ community

/// `PATCH /guilds/{id}` body turning Community on: the features Discord
/// reported plus `COMMUNITY`, the two channels, a verification level of at
/// least Low and the explicit content filter for all members.
pub fn enable_community_body(
    features: &[String],
    rules: Option<Snowflake>,
    updates: Option<Snowflake>,
    verification_level: u8,
) -> Result<Value, CommunityError> {
    let (Some(rules), Some(updates)) = (rules, updates) else {
        return Err(CommunityError::NeedsChannels);
    };
    let mut features = features.to_vec();
    if !features.iter().any(|f| f == "COMMUNITY") {
        features.push("COMMUNITY".to_owned());
    }
    Ok(json!({
        "features": features,
        "rules_channel_id": rules.to_string(),
        "public_updates_channel_id": updates.to_string(),
        "verification_level": verification_level.clamp(1, 4),
        "explicit_content_filter": 2,
    }))
}

/// The reverse: the reported features without `COMMUNITY`, and no rules or
/// updates channel.
pub fn disable_community_body(features: &[String]) -> Value {
    json!({
        "features": features.iter().filter(|f| *f != "COMMUNITY").collect::<Vec<_>>(),
        "rules_channel_id": null,
        "public_updates_channel_id": null,
    })
}

// ----------------------------------------------------------------- REST

fn write(method: Method, guild: Snowflake, rest: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(
        Route::new(method, &format!("/guilds/{guild}{rest}"))?,
        Priority::UserWrite,
    )
    .with_error_detail())
}

fn read(guild: Snowflake, rest: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(
        Route::new(Method::GET, &format!("/guilds/{guild}{rest}"))?,
        Priority::UserRead,
    )
    .with_error_detail())
}

/// A server that has not configured something answers 404; the editor then
/// starts from nothing.
fn absent(error: &RestError) -> bool {
    matches!(error, RestError::ResourceGone)
        || matches!(error, RestError::Refused(r) if r.status.as_u16() == 404)
}

impl<C: Clock> RestClient<C> {
    /// The rules form for editing; `None` when the server has none.
    pub async fn screening_for_edit(
        &self,
        guild: Snowflake,
    ) -> Result<Option<Screening>, RestError> {
        match self.member_verification(guild).await {
            Ok(screening) => Ok(Some(screening)),
            Err(error) if absent(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Saves the rules (`PATCH /guilds/{id}/member-verification`). The answer
    /// is the form as Discord now holds it, when it can be read.
    pub async fn save_screening(
        &self,
        guild: Snowflake,
        current: Option<&Screening>,
        edit: &RulesEdit,
    ) -> Result<Option<Screening>, CommunityRequestError> {
        let body = screening_body(current, edit).map_err(CommunityRequestError::Invalid)?;
        let request = write(Method::PATCH, guild, "/member-verification")?.json(&body)?;
        let response = self.execute(request).await?;
        Ok(Screening::from_json(response.body()))
    }

    pub async fn onboarding_for_edit(&self, guild: Snowflake) -> Result<OnboardingDoc, RestError> {
        let response = self.execute(read(guild, "/onboarding")?).await?;
        OnboardingDoc::from_json(response.body()).ok_or(RestError::InvalidJson)
    }

    /// Saves the onboarding (`PUT /guilds/{id}/onboarding`), the whole object.
    pub async fn save_onboarding(
        &self,
        guild: Snowflake,
        doc: &OnboardingDoc,
        baseline: &OnboardingDoc,
        known: &Known<'_>,
    ) -> Result<Option<OnboardingDoc>, CommunityRequestError> {
        let body = doc
            .body(baseline, known)
            .map_err(CommunityRequestError::Invalid)?;
        let request = write(Method::PUT, guild, "/onboarding")?.json(&body)?;
        let response = self.execute(request).await?;
        Ok(OnboardingDoc::from_json(response.body()))
    }

    /// The guide for editing; an empty one when the server has none.
    pub async fn guide_for_edit(&self, guild: Snowflake) -> Result<GuideDoc, RestError> {
        match self.execute(read(guild, "/new-member-welcome")?).await {
            Ok(response) => GuideDoc::from_json(response.body()).ok_or(RestError::InvalidJson),
            Err(error) if absent(&error) => Ok(GuideDoc::empty()),
            Err(error) => Err(error),
        }
    }

    /// Saves the guide (`PUT /guilds/{id}/new-member-welcome`).
    pub async fn save_guide(
        &self,
        guild: Snowflake,
        doc: &GuideDoc,
        baseline: &GuideDoc,
        channels: &[Snowflake],
        me: Snowflake,
    ) -> Result<Option<GuideDoc>, CommunityRequestError> {
        let body = doc
            .body(baseline, channels, me)
            .map_err(CommunityRequestError::Invalid)?;
        let request = write(Method::PUT, guild, "/new-member-welcome")?.json(&body)?;
        let response = self.execute(request).await?;
        Ok(GuideDoc::from_json(response.body()))
    }

    /// Turns Community on or off (`PATCH /guilds/{id}`); the display follows
    /// `GUILD_UPDATE`.
    pub async fn set_community(
        &self,
        guild: Snowflake,
        body: &Value,
    ) -> Result<(), CommunityRequestError> {
        let request = write(Method::PATCH, guild, "")?.json(body)?;
        self.execute(request).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn onboarding() -> OnboardingDoc {
        OnboardingDoc::from_json(
            include_bytes!("../../../fixtures/rest/onboarding-synthetic.json").as_slice(),
        )
        .unwrap()
    }

    fn guide() -> GuideDoc {
        GuideDoc::from_json(
            include_bytes!("../../../fixtures/rest/new-member-welcome-synthetic.json").as_slice(),
        )
        .unwrap()
    }

    fn screening(name: &str) -> Screening {
        let all: Value = serde_json::from_str(include_str!(
            "../../../fixtures/rest/member-verification-synthetic.json"
        ))
        .unwrap();
        Screening::from_json(all[name].to_string().as_bytes()).unwrap()
    }

    fn sf(ids: &[u64]) -> Vec<Snowflake> {
        ids.iter().copied().map(Snowflake).collect()
    }

    struct World {
        channels: Vec<Snowflake>,
        roles: Vec<Snowflake>,
        assignable: Vec<Snowflake>,
        default_ok: Vec<Snowflake>,
    }

    impl World {
        fn new() -> Self {
            Self {
                channels: sf(&[201, 202, 203, 204, 205]),
                roles: sf(&[401, 402, 403, 404, 405, 406]),
                assignable: sf(&[401, 402, 403, 404, 405]),
                default_ok: sf(&[201, 202]),
            }
        }
        fn known(&self) -> Known<'_> {
            Known {
                channels: &self.channels,
                roles: &self.roles,
                assignable: &self.assignable,
                default_ok: &self.default_ok,
            }
        }
    }

    #[test]
    fn rules_replace_only_the_terms_and_keep_other_fields_and_the_version() {
        let current = screening("with_question");
        let edit = RulesEdit {
            enabled: true,
            description: " Hello ".into(),
            rules: vec!["One.".into(), "  ".into(), "Two.".into()],
        };
        let body = screening_body(Some(&current), &edit).unwrap();
        assert_eq!(body["version"], "2030-01-02T00:00:00.000000+00:00");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["description"], "Hello");
        let fields = body["form_fields"].as_array().unwrap();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields[0]["values"], json!(["One.", "Two."]));
        assert_eq!(fields[1]["field_type"], "PARAGRAPH");
        assert_eq!(fields[1]["placeholder"], "Tell us");
        assert_eq!(RulesEdit::other_fields(&current).len(), 2);
    }

    #[test]
    fn rules_start_a_new_form_without_a_version_and_drop_emptied_terms() {
        let edit = RulesEdit {
            enabled: true,
            description: String::new(),
            rules: vec!["Be kind.".into()],
        };
        let body = screening_body(None, &edit).unwrap();
        assert!(body.get("version").is_none());
        assert_eq!(body["description"], Value::Null);
        assert_eq!(body["form_fields"][0]["field_type"], "TERMS");
        assert_eq!(body["form_fields"][0]["required"], true);

        let current = screening("terms");
        let cleared = RulesEdit {
            rules: Vec::new(),
            ..RulesEdit::from_screening(Some(&current))
        };
        let body = screening_body(Some(&current), &cleared).unwrap();
        assert_eq!(body["form_fields"], json!([]));
        assert_eq!(RulesEdit::from_screening(Some(&current)).rules.len(), 3);
        assert!(RulesEdit::from_screening(Some(&current)).enabled);
    }

    #[test]
    fn rules_are_bounded() {
        let mut edit = RulesEdit::from_screening(None);
        edit.rules = (0..17).map(|i| format!("Rule {i}")).collect();
        assert_eq!(
            screening_body(None, &edit),
            Err(CommunityError::TooManyRules)
        );
        edit.rules = vec!["x".repeat(MAX_RULE_CHARS + 1)];
        assert_eq!(
            screening_body(None, &edit),
            Err(CommunityError::RuleTooLong)
        );
        edit.rules = vec!["x".repeat(MAX_RULE_CHARS)];
        assert!(screening_body(None, &edit).is_ok());
        edit.description = "d".repeat(MAX_RULES_DESCRIPTION_CHARS + 1);
        assert_eq!(
            screening_body(None, &edit),
            Err(CommunityError::DescriptionTooLong)
        );
    }

    #[test]
    fn an_unchanged_onboarding_round_trips_with_unrelated_fields() {
        let doc = onboarding();
        let world = World::new();
        let body = doc.body(&doc, &world.known()).unwrap();
        assert_eq!(body["guild_id"], "100");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["default_channel_ids"], json!(["201", "202"]));
        let prompts = body["prompts"].as_array().unwrap();
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts[0]["type"], 0);
        assert_eq!(prompts[0]["options"][0]["emoji_name"], "♟️");
        assert_eq!(prompts[0]["options"][0]["emoji"]["name"], "♟️");
        // A custom emoji is kept as it was.
        assert_eq!(prompts[0]["options"][1]["emoji_id"], "900");
        assert_eq!(prompts[0]["options"][1]["emoji"]["id"], "900");
        assert_eq!(prompts[0]["options"][1]["role_ids"], json!(["402", "403"]));
        assert_eq!(prompts[1]["options"][1]["description"], Value::Null);
    }

    #[test]
    fn onboarding_edits_add_prompts_with_generated_ids() {
        let baseline = onboarding();
        let mut doc = baseline.clone();
        doc.mode = 1;
        let mut prompt = PromptDoc::new(fresh_id(1_800_000_000_000, 1));
        prompt.title = " Pets ".into();
        prompt.single_select = true;
        let mut option = OptionDoc::new(fresh_id(1_800_000_000_000, 2));
        option.title = "Cats".into();
        option.emoji = option.emoji.typed("🐱");
        option.role_ids = sf(&[405]);
        option.channel_ids = sf(&[205]);
        prompt.options.push(option);
        doc.prompts.push(prompt);
        let world = World::new();
        let body = doc.body(&baseline, &world.known()).unwrap();
        assert_eq!(body["mode"], 1);
        let added = &body["prompts"][3];
        assert_eq!(added["title"], "Pets");
        assert_eq!(added["type"], 0);
        assert_eq!(added["single_select"], true);
        assert_eq!(added["options"][0]["emoji_name"], "🐱");
        let id: u64 = added["id"].as_str().unwrap().parse().unwrap();
        assert_eq!(id >> 22, 1_800_000_000_000 - DISCORD_EPOCH_MS);
        assert_ne!(added["id"], added["options"][0]["id"]);
    }

    #[test]
    fn onboarding_references_the_account_cannot_make_are_caught() {
        let baseline = onboarding();
        let world = World::new();
        let try_doc = |edit: &dyn Fn(&mut OnboardingDoc)| {
            let mut doc = baseline.clone();
            edit(&mut doc);
            doc.body(&baseline, &world.known())
        };
        // A role above the account, an unknown role, an unseen channel.
        assert_eq!(
            try_doc(&|d| d.prompts[0].options[0].role_ids.push(Snowflake(406))),
            Err(CommunityError::UnassignableRole(Snowflake(406)))
        );
        assert_eq!(
            try_doc(&|d| d.prompts[0].options[0].role_ids.push(Snowflake(999))),
            Err(CommunityError::UnknownRole(Snowflake(999)))
        );
        assert_eq!(
            try_doc(&|d| d.prompts[0].options[0].channel_ids.push(Snowflake(999))),
            Err(CommunityError::UnknownChannel(Snowflake(999)))
        );
        // A default channel @everyone cannot view, or one nobody can see.
        assert_eq!(
            try_doc(&|d| d.default_channels.push(Snowflake(203))),
            Err(CommunityError::NotEveryoneVisible(Snowflake(203)))
        );
        assert_eq!(
            try_doc(&|d| d.default_channels.push(Snowflake(999))),
            Err(CommunityError::UnknownChannel(Snowflake(999)))
        );
        // What the saved document already holds is not rejected.
        let mut old = baseline.clone();
        old.prompts[0].options[0].role_ids.push(Snowflake(406));
        old.default_channels.push(Snowflake(999));
        assert!(old.body(&old, &world.known()).is_ok());
        // Structure.
        assert_eq!(
            try_doc(&|d| d.prompts[0].title = " ".into()),
            Err(CommunityError::EmptyTitle)
        );
        assert_eq!(
            try_doc(&|d| d.prompts[0].options.clear()),
            Err(CommunityError::EmptyPrompt)
        );
        assert_eq!(
            try_doc(&|d| d.prompts[0].options[0].emoji = Emoji::Unicode("smile".into())),
            Err(CommunityError::BadEmoji)
        );
        assert_eq!(
            try_doc(&|d| d.prompts[0].options[0].title = "t".repeat(51)),
            Err(CommunityError::TitleTooLong)
        );
    }

    #[test]
    fn onboarding_counts_are_bounded_and_a_cut_document_is_read_only() {
        let baseline = onboarding();
        let world = World::new();
        let mut doc = baseline.clone();
        while doc.prompts.len() <= MAX_PROMPTS_EDITED {
            doc.prompts.push(doc.prompts[0].clone());
        }
        assert_eq!(
            doc.body(&baseline, &world.known()),
            Err(CommunityError::TooManyPrompts)
        );
        let mut doc = baseline.clone();
        while doc.prompts[0].options.len() <= MAX_OPTIONS_EDITED {
            let copy = doc.prompts[0].options[0].clone();
            doc.prompts[0].options.push(copy);
        }
        assert_eq!(
            doc.body(&baseline, &world.known()),
            Err(CommunityError::TooManyOptions)
        );

        let many: Vec<Value> = (0..16)
            .map(|i| json!({"id": (1000 + i).to_string(), "title": "p", "options": [{"id": (2000 + i).to_string(), "title": "o"}]}))
            .collect();
        let big = json!({"enabled": true, "prompts": many}).to_string();
        let cut = OnboardingDoc::from_json(big.as_bytes()).unwrap();
        assert!(!cut.complete() && cut.prompts.len() == MAX_PROMPTS_EDITED);
        assert_eq!(
            cut.body(&cut, &world.known()),
            Err(CommunityError::NotEditable)
        );
        // An entry that cannot be read is an error, never silently dropped.
        assert!(OnboardingDoc::from_json(br#"{"prompts":[{"title":"no id"}]}"#).is_none());
    }

    #[test]
    fn the_guide_keeps_authors_actions_and_unrelated_fields() {
        let baseline = guide();
        let channels = sf(&[201, 202, 999]);
        let body = baseline.body(&baseline, &channels, Snowflake(7)).unwrap();
        assert_eq!(body["enabled"], true);
        assert_eq!(body["welcome_message"]["author_ids"], json!(["500"]));
        assert_eq!(body["new_member_actions"][0]["action_type"], 0);
        assert_eq!(body["new_member_actions"][0]["emoji_name"], "👋");
        assert_eq!(body["new_member_actions"][1]["action_type"], 1);
        assert_eq!(body["resource_channels"][0]["title"], "Rules and news");

        let mut edit = baseline.clone();
        edit.welcome = " Hi ".into();
        edit.authors.clear();
        let mut action = GuideEntry::new(Snowflake(202));
        action.title = "Read".into();
        edit.actions.push(action);
        let body = edit.body(&baseline, &channels, Snowflake(7)).unwrap();
        assert_eq!(body["welcome_message"]["message"], "Hi");
        assert_eq!(body["welcome_message"]["author_ids"], json!(["7"]));
        assert_eq!(body["new_member_actions"][2]["title"], "Read");
        assert!(body["new_member_actions"][2].get("action_type").is_none());
    }

    #[test]
    fn the_guide_is_bounded_and_checks_channels() {
        let baseline = guide();
        let channels = sf(&[201, 202]);
        let mut edit = baseline.clone();
        edit.welcome = "w".repeat(MAX_WELCOME_CHARS + 1);
        assert_eq!(
            edit.body(&baseline, &channels, Snowflake(7)),
            Err(CommunityError::WelcomeTooLong)
        );
        let mut edit = GuideDoc::empty();
        let entry = |t: &str| {
            let mut e = GuideEntry::new(Snowflake(201));
            e.title = t.into();
            e
        };
        for i in 0..=MAX_NEW_MEMBER_ACTIONS {
            edit.actions.push(entry(&format!("a{i}")));
        }
        assert_eq!(
            edit.body(&GuideDoc::empty(), &channels, Snowflake(7)),
            Err(CommunityError::TooManyActions)
        );
        edit.actions.truncate(1);
        for i in 0..=MAX_RESOURCE_CHANNELS {
            edit.resources.push(entry(&format!("r{i}")));
        }
        assert_eq!(
            edit.body(&GuideDoc::empty(), &channels, Snowflake(7)),
            Err(CommunityError::TooManyResources)
        );
        edit.resources.truncate(1);
        edit.resources[0].channel = Snowflake(999);
        assert_eq!(
            edit.body(&GuideDoc::empty(), &channels, Snowflake(7)),
            Err(CommunityError::UnknownChannel(Snowflake(999)))
        );
        edit.resources[0].channel = Snowflake(201);
        edit.resources[0].title.clear();
        assert_eq!(
            edit.body(&GuideDoc::empty(), &channels, Snowflake(7)),
            Err(CommunityError::EmptyTitle)
        );
    }

    #[test]
    fn community_adds_to_the_features_and_the_reverse_removes_only_its_own() {
        let features = vec!["NEWS".to_owned(), "INVITE_SPLASH".to_owned()];
        let body =
            enable_community_body(&features, Some(Snowflake(1)), Some(Snowflake(2)), 0).unwrap();
        assert_eq!(
            body["features"],
            json!(["NEWS", "INVITE_SPLASH", "COMMUNITY"])
        );
        assert_eq!(body["rules_channel_id"], "1");
        assert_eq!(body["public_updates_channel_id"], "2");
        assert_eq!(body["verification_level"], 1);
        assert_eq!(body["explicit_content_filter"], 2);
        let body =
            enable_community_body(&features, Some(Snowflake(1)), Some(Snowflake(2)), 3).unwrap();
        assert_eq!(body["verification_level"], 3);
        assert_eq!(
            enable_community_body(&features, None, Some(Snowflake(2)), 1),
            Err(CommunityError::NeedsChannels)
        );
        let on = vec!["COMMUNITY".to_owned(), "NEWS".to_owned()];
        let body = disable_community_body(&on);
        assert_eq!(body["features"], json!(["NEWS"]));
        assert_eq!(body["rules_channel_id"], Value::Null);
        let again = enable_community_body(&on, Some(Snowflake(1)), Some(Snowflake(2)), 1).unwrap();
        assert_eq!(again["features"], json!(["COMMUNITY", "NEWS"]));
    }

    #[test]
    fn emoji_are_unicode_only_and_custom_ones_survive_an_untouched_field() {
        let custom = Emoji::Custom {
            id: Snowflake(9),
            name: "stone".into(),
            animated: false,
        };
        assert_eq!(custom.typed(""), custom);
        assert_eq!(custom.typed("🙂"), Emoji::Unicode("🙂".into()));
        assert_eq!(Emoji::Unicode("🙂".into()).typed(" "), Emoji::None);
        assert!(Emoji::Unicode("1️⃣".into()).check().is_ok());
        assert!(Emoji::Unicode("a".into()).check().is_err());
        assert!(Emoji::Unicode(":smile:".into()).check().is_err());
        assert_eq!(custom.text(), "");
    }
}
