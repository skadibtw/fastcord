//! Interactions a user starts by hand: pressing a message button, choosing in
//! a select, running a slash command, answering a modal (SPEC §9,
//! docs/PROTOCOL.md). Nothing here is documented for user accounts; every
//! route and field is **unverified** and recorded from community knowledge of
//! the official client.
//!
//! * `POST /interactions` with a JSON body ([`InteractionRequest`]); Discord
//!   answers `204` and reports the outcome over the Gateway
//!   (`INTERACTION_SUCCESS` / `INTERACTION_FAILURE`, matched by the request's
//!   `nonce`). The official client sends the same fields as multipart
//!   `payload_json`.
//! * `GET /guilds/{id}/application-command-index` and
//!   `GET /channels/{id}/application-command-index` list the commands the
//!   account can use ([`CommandIndex`]), parsed with bounds.
//! * `INTERACTION_MODAL_CREATE` opens a modal ([`InteractionModal`]).
//!
//! Each request is sent once; the caller never retries one.

use std::fmt;

use fastcord_model::Snowflake;
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Clock, Method, Nonce, Priority, RestClient, RestError, RestRequest, Route};

pub const TYPE_COMMAND: u8 = 2;
pub const TYPE_COMPONENT: u8 = 3;
pub const TYPE_MODAL_SUBMIT: u8 = 5;

/// Commands one index keeps.
pub const MAX_COMMANDS: usize = 200;
pub const MAX_COMMAND_OPTIONS: usize = 25;
pub const MAX_OPTION_CHOICES: usize = 25;
pub const MAX_COMMAND_NAME_CHARS: usize = 32;
pub const MAX_COMMAND_TEXT_CHARS: usize = 100;
/// Heap bytes one parsed index may hold; commands past it are dropped.
pub const MAX_INDEX_BYTES: usize = 256 * 1024;
/// Inputs one modal keeps (Discord allows 5).
pub const MAX_MODAL_INPUTS: usize = 5;
pub const MAX_MODAL_TEXT_CHARS: usize = 4000;

/// Option types (Discord's application command option types).
pub const OPTION_SUB_COMMAND: u8 = 1;
pub const OPTION_SUB_COMMAND_GROUP: u8 = 2;
pub const OPTION_STRING: u8 = 3;
pub const OPTION_INTEGER: u8 = 4;
pub const OPTION_BOOLEAN: u8 = 5;
pub const OPTION_USER: u8 = 6;
pub const OPTION_CHANNEL: u8 = 7;
pub const OPTION_ROLE: u8 = 8;
pub const OPTION_MENTIONABLE: u8 = 9;
pub const OPTION_NUMBER: u8 = 10;
pub const OPTION_ATTACHMENT: u8 = 11;

/// Where a request is made from.
pub struct InteractionContext<'a> {
    pub session_id: &'a str,
    pub guild_id: Option<Snowflake>,
    pub channel_id: Snowflake,
    pub nonce: Nonce,
}

/// The JSON body of `POST /interactions`. `Debug` shows only the type.
#[derive(Serialize, Clone, PartialEq)]
pub struct InteractionRequest {
    #[serde(rename = "type")]
    kind: u8,
    application_id: Snowflake,
    #[serde(skip_serializing_if = "Option::is_none")]
    guild_id: Option<Snowflake>,
    channel_id: Snowflake,
    session_id: String,
    nonce: Nonce,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<Snowflake>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message_flags: Option<u64>,
    data: Value,
}

impl fmt::Debug for InteractionRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractionRequest")
            .field("type", &self.kind)
            .finish_non_exhaustive()
    }
}

/// Why a typed command argument cannot be sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgError {
    Missing,
    NotAnInteger,
    NotANumber,
    NotYesOrNo,
    /// A user, channel, role or mentionable needs an ID or a mention.
    NotAnId,
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Missing => "a required option is empty",
            Self::NotAnInteger => "an option needs a whole number",
            Self::NotANumber => "an option needs a number",
            Self::NotYesOrNo => "an option needs yes or no",
            Self::NotAnId => "an option needs an ID or a mention",
        })
    }
}

impl std::error::Error for ArgError {}

/// The ID in `123`, `<@123>`, `<@!123>`, `<#123>` or `<@&123>`.
fn id_of(text: &str) -> Option<Snowflake> {
    let text = text.trim();
    let digits = text
        .strip_prefix("<@&")
        .or_else(|| text.strip_prefix("<@!"))
        .or_else(|| text.strip_prefix("<@"))
        .or_else(|| text.strip_prefix("<#"))
        .map_or(text, |inner| inner.strip_suffix('>').unwrap_or(inner));
    digits.parse().ok()
}

/// The JSON value of what the user typed for an option of `kind`.
pub fn arg_value(kind: u8, text: &str) -> Result<Value, ArgError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(ArgError::Missing);
    }
    Ok(match kind {
        OPTION_INTEGER => json!(text.parse::<i64>().map_err(|_| ArgError::NotAnInteger)?),
        OPTION_NUMBER => {
            let number = text.parse::<f64>().map_err(|_| ArgError::NotANumber)?;
            serde_json::Number::from_f64(number)
                .map(Value::Number)
                .ok_or(ArgError::NotANumber)?
        }
        OPTION_BOOLEAN => match text.to_ascii_lowercase().as_str() {
            "true" | "yes" | "y" | "on" => Value::Bool(true),
            "false" | "no" | "n" | "off" => Value::Bool(false),
            _ => return Err(ArgError::NotYesOrNo),
        },
        OPTION_USER | OPTION_CHANNEL | OPTION_ROLE | OPTION_MENTIONABLE => {
            json!(id_of(text).ok_or(ArgError::NotAnId)?.to_string())
        }
        _ => json!(text),
    })
}

/// One typed option of a command being run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandArg {
    pub name: String,
    pub kind: u8,
    /// What the user typed; empty for an optional option they left out.
    pub text: String,
}

fn options_json(args: &[CommandArg]) -> Result<Vec<Value>, ArgError> {
    let mut options = Vec::new();
    for arg in args {
        if arg.text.trim().is_empty() {
            continue;
        }
        options.push(json!({
            "type": arg.kind,
            "name": arg.name,
            "value": arg_value(arg.kind, &arg.text)?,
        }));
    }
    Ok(options)
}

impl InteractionRequest {
    fn new(
        kind: u8,
        application_id: Snowflake,
        context: &InteractionContext<'_>,
        data: Value,
    ) -> Self {
        Self {
            kind,
            application_id,
            guild_id: context.guild_id,
            channel_id: context.channel_id,
            session_id: context.session_id.to_owned(),
            nonce: context.nonce,
            message_id: None,
            message_flags: None,
            data,
        }
    }

    /// A button press (`values` empty) or a select choice on a message.
    #[allow(clippy::too_many_arguments)]
    pub fn component(
        context: &InteractionContext<'_>,
        application_id: Snowflake,
        message: Snowflake,
        message_flags: u64,
        component_type: u8,
        custom_id: &str,
        values: &[String],
    ) -> Self {
        let mut data = json!({"component_type": component_type, "custom_id": custom_id});
        if component_type != 2 {
            data["values"] = json!(values);
        }
        let mut request = Self::new(TYPE_COMPONENT, application_id, context, data);
        request.message_id = Some(message);
        request.message_flags = Some(message_flags);
        request
    }

    /// A slash command, optionally through one subcommand.
    pub fn command(
        context: &InteractionContext<'_>,
        command: &AppCommand,
        subcommand: Option<&str>,
        args: &[CommandArg],
    ) -> Result<Self, ArgError> {
        let mut options = options_json(args)?;
        if let Some(sub) = subcommand {
            options = vec![json!({"type": OPTION_SUB_COMMAND, "name": sub, "options": options})];
        }
        let data = json!({
            "version": command.version.to_string(),
            "id": command.id.to_string(),
            "name": command.name,
            "type": 1,
            "options": options,
        });
        Ok(Self::new(
            TYPE_COMMAND,
            command.application_id,
            context,
            data,
        ))
    }

    /// The answers to a modal.
    pub fn modal_submit(
        context: &InteractionContext<'_>,
        modal: &InteractionModal,
        values: &[String],
    ) -> Self {
        let inputs: Vec<Value> = modal
            .inputs
            .iter()
            .zip(values)
            .map(|(input, value)| {
                json!({"type": 1, "components": [
                    {"type": 4, "custom_id": input.custom_id, "value": value}
                ]})
            })
            .collect();
        let data = json!({
            "id": modal.id.to_string(),
            "custom_id": modal.custom_id,
            "components": inputs,
        });
        Self::new(TYPE_MODAL_SUBMIT, modal.application_id, context, data)
    }
}

/// A text input of a modal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModalInput {
    pub custom_id: String,
    pub label: String,
    /// Style 2 (multi-line) rather than 1.
    pub paragraph: bool,
    pub min_length: usize,
    pub max_length: usize,
    pub required: bool,
    pub placeholder: String,
    /// The text the input starts with.
    pub value: String,
}

impl ModalInput {
    /// Why `text` cannot be submitted, if it cannot.
    pub fn check(&self, text: &str) -> Option<&'static str> {
        let length = text.chars().count();
        if length == 0 && !self.required {
            return None;
        }
        if length == 0 {
            return Some("A required field is empty.");
        }
        if length < self.min_length {
            return Some("A field is shorter than the app allows.");
        }
        if length > self.max_length {
            return Some("A field is longer than the app allows.");
        }
        None
    }
}

/// `INTERACTION_MODAL_CREATE`: the app asks the user to fill in a form.
#[derive(Clone, PartialEq, Eq)]
pub struct InteractionModal {
    /// The interaction this modal answers; the submission names it.
    pub id: Snowflake,
    pub nonce: Option<String>,
    pub application_id: Snowflake,
    pub channel_id: Option<Snowflake>,
    pub title: String,
    pub custom_id: String,
    pub inputs: Vec<ModalInput>,
}

impl fmt::Debug for InteractionModal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractionModal")
            .field("inputs", &self.inputs.len())
            .finish_non_exhaustive()
    }
}

fn text(value: &Value, key: &str, max: usize) -> String {
    let text = value.get(key).and_then(Value::as_str).unwrap_or_default();
    match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_owned(),
        None => text.to_owned(),
    }
}

fn snowflake(value: &Value, key: &str) -> Option<Snowflake> {
    match value.get(key)? {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64().map(Snowflake),
        _ => None,
    }
}

fn modal_input(value: &Value) -> Option<ModalInput> {
    // A Components V2 modal wraps its input in a label (type 18).
    let (value, label) = match value.get("type").and_then(Value::as_u64) {
        Some(18) => {
            let inner = value.get("component")?;
            (inner, text(value, "label", 45))
        }
        _ => (value, text(value, "label", 45)),
    };
    if value.get("type").and_then(Value::as_u64) != Some(4) {
        return None;
    }
    let length = |key: &str, default: usize| {
        value
            .get(key)
            .and_then(Value::as_u64)
            .map_or(default, |n| (n as usize).min(MAX_MODAL_TEXT_CHARS))
    };
    Some(ModalInput {
        custom_id: text(value, "custom_id", 100),
        label,
        paragraph: value.get("style").and_then(Value::as_u64) == Some(2),
        min_length: length("min_length", 0),
        max_length: length("max_length", MAX_MODAL_TEXT_CHARS),
        required: value
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        placeholder: text(value, "placeholder", 100),
        value: text(value, "value", MAX_MODAL_TEXT_CHARS),
    })
}

/// Parses an `INTERACTION_MODAL_CREATE` payload; `None` when it cannot be
/// answered (no id, no application, no input).
pub fn parse_modal(raw: &str) -> Option<InteractionModal> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let id = snowflake(&value, "id")?;
    let application_id = snowflake(&value, "application_id")
        .or_else(|| snowflake(value.get("application")?, "id"))?;
    let mut inputs = Vec::new();
    for row in value.get("components")?.as_array()? {
        if let Some(items) = row.get("components").and_then(Value::as_array) {
            inputs.extend(items.iter().filter_map(modal_input));
        } else {
            inputs.extend(modal_input(row));
        }
    }
    inputs.truncate(MAX_MODAL_INPUTS);
    if inputs.is_empty() || inputs.iter().any(|input| input.custom_id.is_empty()) {
        return None;
    }
    Some(InteractionModal {
        id,
        nonce: value
            .get("nonce")
            .and_then(Value::as_str)
            .map(|nonce| nonce.chars().take(25).collect()),
        application_id,
        channel_id: snowflake(&value, "channel_id"),
        title: text(&value, "title", 45),
        custom_id: text(&value, "custom_id", 100),
        inputs,
    })
}

/// `INTERACTION_SUCCESS` or `INTERACTION_FAILURE`: how Discord settled the
/// interaction with this nonce.
#[derive(Clone, PartialEq, Eq)]
pub struct InteractionOutcome {
    pub nonce: String,
    pub id: Option<Snowflake>,
    /// Discord's reason code of a failure.
    pub reason: Option<u32>,
}

impl fmt::Debug for InteractionOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InteractionOutcome")
            .field("reason", &self.reason)
            .finish_non_exhaustive()
    }
}

pub fn parse_outcome(raw: &str) -> Option<InteractionOutcome> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let nonce = value.get("nonce")?.as_str()?;
    if nonce.is_empty() || nonce.len() > 25 {
        return None;
    }
    Some(InteractionOutcome {
        nonce: nonce.to_owned(),
        id: snowflake(&value, "id"),
        reason: value
            .get("reason_code")
            .and_then(Value::as_u64)
            .and_then(|code| u32::try_from(code).ok()),
    })
}

/// A choice an option offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OptionChoice {
    pub name: String,
    pub value: String,
}

/// An option of a command or subcommand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOption {
    pub kind: u8,
    pub name: String,
    pub description: String,
    pub required: bool,
    pub choices: Vec<OptionChoice>,
    /// Options of a subcommand.
    pub options: Vec<CommandOption>,
}

/// A chat-input (slash) command the account can use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppCommand {
    pub id: Snowflake,
    pub application_id: Snowflake,
    pub version: Snowflake,
    pub name: String,
    pub description: String,
    /// The app's name, when the index lists it.
    pub application: String,
    pub options: Vec<CommandOption>,
}

impl AppCommand {
    fn heap_bytes(&self) -> usize {
        fn options(list: &[CommandOption]) -> usize {
            size_of_val(list)
                + list
                    .iter()
                    .map(|o| {
                        o.name.capacity()
                            + o.description.capacity()
                            + o.choices.capacity() * size_of::<OptionChoice>()
                            + o.choices
                                .iter()
                                .map(|c| c.name.capacity() + c.value.capacity())
                                .sum::<usize>()
                            + options(&o.options)
                    })
                    .sum::<usize>()
        }
        size_of::<Self>()
            + self.name.capacity()
            + self.description.capacity()
            + self.application.capacity()
            + options(&self.options)
    }
}

/// The commands usable in one scope, bounded to [`MAX_COMMANDS`] and
/// [`MAX_INDEX_BYTES`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandIndex {
    pub commands: Vec<AppCommand>,
    bytes: usize,
}

impl CommandIndex {
    /// Heap bytes held, for the cache's budget.
    pub fn heap_bytes(&self) -> usize {
        self.bytes + self.commands.capacity() * size_of::<AppCommand>()
    }

    /// Commands whose name starts with `prefix` (case-insensitive), in the
    /// index's order, at most `limit`.
    pub fn matching<'a>(&'a self, prefix: &str, limit: usize) -> Vec<&'a AppCommand> {
        let prefix = prefix.to_lowercase();
        self.commands
            .iter()
            .filter(|c| c.name.to_lowercase().starts_with(&prefix))
            .take(limit)
            .collect()
    }
}

fn parse_option(value: &Value, depth: usize) -> Option<CommandOption> {
    let kind = u8::try_from(value.get("type")?.as_u64()?).ok()?;
    let name = text(value, "name", MAX_COMMAND_NAME_CHARS);
    if name.is_empty() {
        return None;
    }
    let choices = value
        .get("choices")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(MAX_OPTION_CHOICES)
                .filter_map(|choice| {
                    let value = match choice.get("value")? {
                        Value::String(s) => s.chars().take(MAX_COMMAND_TEXT_CHARS).collect(),
                        Value::Number(n) => n.to_string(),
                        _ => return None,
                    };
                    Some(OptionChoice {
                        name: text(choice, "name", MAX_COMMAND_TEXT_CHARS),
                        value,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let options = if depth == 0 {
        parse_options(value, depth + 1)
    } else {
        Vec::new()
    };
    Some(CommandOption {
        kind,
        name,
        description: text(value, "description", MAX_COMMAND_TEXT_CHARS),
        required: value
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        choices,
        options,
    })
}

fn parse_options(value: &Value, depth: usize) -> Vec<CommandOption> {
    value
        .get("options")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(MAX_COMMAND_OPTIONS)
                .filter_map(|item| parse_option(item, depth))
                .collect()
        })
        .unwrap_or_default()
}

/// Whether this client can fill in and send every option of `options`
/// (subcommand groups and required attachments it cannot).
fn supported(options: &[CommandOption]) -> bool {
    options.iter().all(|option| match option.kind {
        OPTION_SUB_COMMAND_GROUP => false,
        OPTION_ATTACHMENT => !option.required,
        OPTION_SUB_COMMAND => supported(&option.options),
        _ => true,
    })
}

/// Parses an `application-command-index` response. Only chat-input commands
/// this client can complete are kept; malformed ones are dropped.
pub fn parse_index(body: &[u8]) -> Result<CommandIndex, RestError> {
    let value: Value = serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
    let apps: Vec<(Snowflake, String)> = value
        .get("applications")
        .and_then(Value::as_array)
        .map(|apps| {
            apps.iter()
                .filter_map(|app| Some((snowflake(app, "id")?, text(app, "name", 40))))
                .collect()
        })
        .unwrap_or_default();
    let mut index = CommandIndex::default();
    let Some(commands) = value.get("application_commands").and_then(Value::as_array) else {
        return Ok(index);
    };
    for item in commands {
        if index.commands.len() == MAX_COMMANDS {
            break;
        }
        if item.get("type").and_then(Value::as_u64).unwrap_or(1) != 1 {
            continue;
        }
        let (Some(id), Some(application_id)) =
            (snowflake(item, "id"), snowflake(item, "application_id"))
        else {
            continue;
        };
        let name = text(item, "name", MAX_COMMAND_NAME_CHARS);
        let mut options = parse_options(item, 0);
        options.retain(|o| o.kind != OPTION_ATTACHMENT || o.required);
        if name.is_empty() || !supported(&options) {
            continue;
        }
        let command = AppCommand {
            id,
            application_id,
            version: snowflake(item, "version").unwrap_or(id),
            name,
            description: text(item, "description", MAX_COMMAND_TEXT_CHARS),
            application: apps
                .iter()
                .find(|(app, _)| *app == application_id)
                .map(|(_, name)| name.clone())
                .unwrap_or_default(),
            options,
        };
        let bytes = command.heap_bytes();
        if index.bytes + bytes > MAX_INDEX_BYTES {
            break;
        }
        index.bytes += bytes;
        index.commands.push(command);
    }
    index.commands.shrink_to_fit();
    Ok(index)
}

/// Where the command list of a scope comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IndexScope {
    Guild(Snowflake),
    /// A direct or group conversation.
    Channel(Snowflake),
}

fn index_route(scope: IndexScope) -> Result<Route, RestError> {
    Route::new(
        Method::GET,
        &match scope {
            IndexScope::Guild(id) => format!("/guilds/{id}/application-command-index"),
            IndexScope::Channel(id) => format!("/channels/{id}/application-command-index"),
        },
    )
}

impl<C: Clock> RestClient<C> {
    /// Sends one interaction. Discord answers `204`; the outcome follows over
    /// the Gateway. Sent once at user-write priority.
    pub async fn send_interaction(&self, request: &InteractionRequest) -> Result<(), RestError> {
        let request = RestRequest::new(
            Route::new(Method::POST, "/interactions")?,
            Priority::UserWrite,
        )
        .json(request)?;
        self.execute(request).await.map(drop)
    }

    /// The commands usable in `scope`.
    pub async fn command_index(&self, scope: IndexScope) -> Result<CommandIndex, RestError> {
        let request = RestRequest::new(index_route(scope)?, Priority::UserRead);
        let response = self.execute(request).await?;
        parse_index(response.body())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INDEX: &str = include_str!("../../../fixtures/rest/application-command-index.json");
    const MODAL: &str = include_str!("../../../fixtures/gateway/interaction_modal_create.json");

    fn nonce() -> Nonce {
        crate::NonceGenerator.next(1_800_000_000_000)
    }

    fn context(nonce: Nonce) -> InteractionContext<'static> {
        InteractionContext {
            session_id: "fixture-session",
            guild_id: Some(Snowflake(100)),
            channel_id: Snowflake(500),
            nonce,
        }
    }

    #[test]
    fn a_button_press_names_the_message_and_a_select_adds_its_values() {
        let nonce = nonce();
        let press = InteractionRequest::component(
            &context(nonce),
            Snowflake(900),
            Snowflake(700),
            64,
            2,
            "yes",
            &[],
        );
        let sent = serde_json::to_value(&press).unwrap();
        assert_eq!(sent["type"], 3);
        assert_eq!(sent["application_id"], "900");
        assert_eq!(sent["guild_id"], "100");
        assert_eq!(sent["channel_id"], "500");
        assert_eq!(sent["message_id"], "700");
        assert_eq!(sent["message_flags"], 64);
        assert_eq!(sent["session_id"], "fixture-session");
        assert_eq!(sent["nonce"], nonce.to_string());
        assert_eq!(
            sent["data"],
            json!({"component_type": 2, "custom_id": "yes"})
        );
        let pick = InteractionRequest::component(
            &context(nonce),
            Snowflake(900),
            Snowflake(700),
            0,
            3,
            "pick",
            &["a".to_owned()],
        );
        assert_eq!(
            serde_json::to_value(&pick).unwrap()["data"],
            json!({"component_type": 3, "custom_id": "pick", "values": ["a"]})
        );
        let shown = format!("{press:?}");
        assert!(!shown.contains("fixture-session") && !shown.contains("yes"));
        let dm = InteractionContext {
            guild_id: None,
            ..context(nonce)
        };
        let sent = serde_json::to_value(InteractionRequest::component(
            &dm,
            Snowflake(9),
            Snowflake(7),
            0,
            2,
            "x",
            &[],
        ))
        .unwrap();
        assert!(sent.get("guild_id").is_none());
    }

    #[test]
    fn the_index_keeps_usable_chat_commands_with_typed_options() {
        let index = parse_index(INDEX.as_bytes()).unwrap();
        let names: Vec<&str> = index.commands.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["roll", "poll", "remind"], "{names:?}");
        let roll = &index.commands[0];
        assert_eq!(roll.application, "Dice");
        assert_eq!(roll.version, Snowflake(1_300_000_000_000_000_011));
        assert_eq!(roll.options.len(), 2);
        assert!(roll.options[0].required && roll.options[0].kind == OPTION_INTEGER);
        assert_eq!(roll.options[1].choices[1].value, "d20");
        let poll = &index.commands[1];
        assert_eq!(poll.options[0].kind, OPTION_SUB_COMMAND);
        assert_eq!(poll.options[0].options[0].name, "question");
        assert!(index.heap_bytes() > 0 && index.heap_bytes() <= MAX_INDEX_BYTES * 2);
        assert_eq!(
            index
                .matching("RO", 10)
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["roll"]
        );
        assert_eq!(index.matching("", 2).len(), 2);
        assert!(parse_index(b"[1]").unwrap().commands.is_empty());
        assert!(parse_index(b"nope").is_err());
    }

    #[test]
    fn the_index_is_bounded_in_count_and_bytes() {
        let commands: Vec<Value> = (1..=300u64)
            .map(|n| {
                json!({"id": n.to_string(), "application_id": "9", "name": format!("c{n}"),
                       "description": "d".repeat(500)})
            })
            .collect();
        let body = serde_json::to_vec(&json!({"application_commands": commands})).unwrap();
        let index = parse_index(&body).unwrap();
        assert_eq!(index.commands.len(), MAX_COMMANDS);
        assert!(index.commands[0].description.chars().count() <= MAX_COMMAND_TEXT_CHARS);
        let wide: Vec<Value> = (1..=200u64)
            .map(|n| {
                let options: Vec<Value> = (0..25)
                    .map(|o| {
                        json!({"type": 3, "name": format!("o{o}"), "description": "x".repeat(100),
                               "choices": (0..25).map(|_| json!({"name": "n".repeat(100), "value": "v".repeat(100)})).collect::<Vec<_>>()})
                    })
                    .collect();
                json!({"id": n.to_string(), "application_id": "9", "name": format!("c{n}"), "options": options})
            })
            .collect();
        let body = serde_json::to_vec(&json!({"application_commands": wide})).unwrap();
        let index = parse_index(&body).unwrap();
        assert!(index.commands.len() < 200);
        assert!(index.heap_bytes() <= MAX_INDEX_BYTES + 64 * 1024);
    }

    #[test]
    fn a_command_is_sent_with_typed_options_and_one_subcommand_level() {
        let index = parse_index(INDEX.as_bytes()).unwrap();
        let args = [
            CommandArg {
                name: "sides".into(),
                kind: OPTION_INTEGER,
                text: " 6 ".into(),
            },
            CommandArg {
                name: "die".into(),
                kind: OPTION_STRING,
                text: "d20".into(),
            },
            CommandArg {
                name: "left_out".into(),
                kind: OPTION_USER,
                text: String::new(),
            },
        ];
        let request =
            InteractionRequest::command(&context(nonce()), &index.commands[0], None, &args)
                .unwrap();
        let sent = serde_json::to_value(&request).unwrap();
        assert_eq!(sent["type"], 2);
        assert_eq!(sent["application_id"], "9000000000000000001");
        assert!(sent.get("message_id").is_none());
        assert_eq!(sent["data"]["id"], "1300000000000000001");
        assert_eq!(sent["data"]["version"], "1300000000000000011");
        assert_eq!(sent["data"]["type"], 1);
        assert_eq!(
            sent["data"]["options"],
            json!([
                {"type": 4, "name": "sides", "value": 6},
                {"type": 3, "name": "die", "value": "d20"}
            ])
        );
        let sub = InteractionRequest::command(
            &context(nonce()),
            &index.commands[1],
            Some("create"),
            &[CommandArg {
                name: "question".into(),
                kind: OPTION_STRING,
                text: "Lunch?".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&sub).unwrap()["data"]["options"],
            json!([{"type": 1, "name": "create", "options": [
                {"type": 3, "name": "question", "value": "Lunch?"}]}])
        );
    }

    #[test]
    fn typed_arguments_are_checked_before_anything_is_sent() {
        assert_eq!(arg_value(OPTION_INTEGER, "x"), Err(ArgError::NotAnInteger));
        assert_eq!(arg_value(OPTION_INTEGER, "-4"), Ok(json!(-4)));
        assert_eq!(arg_value(OPTION_NUMBER, "1.5"), Ok(json!(1.5)));
        assert_eq!(arg_value(OPTION_NUMBER, "NaN"), Err(ArgError::NotANumber));
        assert_eq!(arg_value(OPTION_BOOLEAN, "Yes"), Ok(json!(true)));
        assert_eq!(
            arg_value(OPTION_BOOLEAN, "maybe"),
            Err(ArgError::NotYesOrNo)
        );
        assert_eq!(arg_value(OPTION_USER, "<@!42>"), Ok(json!("42")));
        assert_eq!(arg_value(OPTION_ROLE, "<@&43>"), Ok(json!("43")));
        assert_eq!(arg_value(OPTION_CHANNEL, "<#44>"), Ok(json!("44")));
        assert_eq!(
            arg_value(OPTION_MENTIONABLE, "name"),
            Err(ArgError::NotAnId)
        );
        assert_eq!(arg_value(OPTION_STRING, "  "), Err(ArgError::Missing));
    }

    #[test]
    fn a_modal_parses_and_its_submission_answers_every_input() {
        let modal = parse_modal(MODAL).unwrap();
        assert_eq!(modal.title, "Report a problem");
        assert_eq!(modal.application_id, Snowflake(9_000_000_000_000_000_001));
        assert_eq!(modal.inputs.len(), 2);
        assert!(modal.inputs[0].required && !modal.inputs[0].paragraph);
        assert!(modal.inputs[1].paragraph && !modal.inputs[1].required);
        assert_eq!(modal.inputs[1].max_length, 300);
        assert_eq!(
            modal.inputs[0].check(""),
            Some("A required field is empty.")
        );
        assert_eq!(
            modal.inputs[0].check("ab"),
            Some("A field is shorter than the app allows.")
        );
        assert_eq!(modal.inputs[1].check(""), None);
        assert!(modal.inputs[1].check(&"x".repeat(301)).is_some());
        let request = InteractionRequest::modal_submit(
            &context(nonce()),
            &modal,
            &["Title".to_owned(), "Body".to_owned()],
        );
        let sent = serde_json::to_value(&request).unwrap();
        assert_eq!(sent["type"], 5);
        assert_eq!(sent["application_id"], "9000000000000000001");
        assert_eq!(sent["data"]["id"], "1310000000000000001");
        assert_eq!(sent["data"]["custom_id"], "report-modal");
        assert_eq!(
            sent["data"]["components"][1],
            json!({"type": 1, "components": [{"type": 4, "custom_id": "details", "value": "Body"}]})
        );
        assert!(parse_modal(r#"{"id":"1","application_id":"2","components":[]}"#).is_none());
        assert!(parse_modal(r#"{"id":"1","components":[]}"#).is_none());
        assert!(!format!("{modal:?}").contains("Report"));
    }

    #[test]
    fn outcomes_need_a_nonce_and_keep_the_reason() {
        let ok = parse_outcome(r#"{"id":"5","nonce":"123"}"#).unwrap();
        assert_eq!(
            (ok.nonce.as_str(), ok.id, ok.reason),
            ("123", Some(Snowflake(5)), None)
        );
        let failed = parse_outcome(r#"{"nonce":"123","reason_code":2}"#).unwrap();
        assert_eq!(failed.reason, Some(2));
        assert!(parse_outcome(r#"{"id":"5"}"#).is_none());
        assert!(parse_outcome(r#"{"nonce":"1234567890123456789012345678"}"#).is_none());
        assert!(parse_outcome("[]").is_none());
    }
}
