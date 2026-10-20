//! Reporting messages, users and servers to Discord (SPEC §16, issue #81).
//!
//! Discord's own client reports through a server-driven menu: a tree of nodes
//! (`GET /reporting/menu/{message|user|guild}`) the person walks, then one
//! submission (`POST /reporting/{message|user|guild}`) naming the target and
//! the chosen path of node IDs. This module parses the menu into a bounded
//! tree (no category is hard-coded) and builds the one submission. Nothing
//! here sends a report by itself: each function is one request, on an
//! explicit click, never retried. Wire shapes are from community
//! documentation of the official client and are unverified against a live
//! account (`docs/TESTING.md`). A node that asks for input this client cannot
//! collect is `Unsupported`; the caller offers the web form instead.
//!
//! Debug output never contains IDs, labels or text of what is reported.

use std::collections::BTreeMap;
use std::fmt;

use fastcord_model::Snowflake;
use reqwest::Method;

use crate::{Clock, Priority, RestClient, RestError, RestRequest, Route};

/// Nodes kept from one menu; a larger menu is refused whole.
pub const MAX_MENU_NODES: usize = 512;
/// Choices kept per node.
pub const MAX_CHOICES: usize = 40;
/// Deepest path walked.
pub const MAX_DEPTH: usize = 12;
const MAX_LABEL_CHARS: usize = 200;
const MAX_TEXT_CHARS: usize = 400;
const MAX_TOKEN_CHARS: usize = 32;
const MAX_RECEIPT_CHARS: usize = 40;

/// What is reported. Voice messages are messages.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ReportTarget {
    Message {
        channel: Snowflake,
        message: Snowflake,
    },
    User(Snowflake),
    Server(Snowflake),
}

impl ReportTarget {
    /// The menu and submission name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Message { .. } => "message",
            Self::User(_) => "user",
            Self::Server(_) => "guild",
        }
    }
}

impl fmt::Debug for ReportTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReportTarget({})", self.name())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    /// Has choices to walk.
    Choice,
    /// A leaf that ends the walk; the report can be submitted from here.
    Submit,
    /// Asks for input this client cannot collect, or is not a way to submit.
    Unsupported,
}

#[derive(Clone, PartialEq, Eq)]
pub struct MenuNode {
    pub header: String,
    pub subheader: String,
    pub choices: Vec<(String, u32)>,
    pub kind: NodeKind,
}

impl fmt::Debug for MenuNode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "MenuNode({:?})", self.kind)
    }
}

/// A parsed, bounded menu.
#[derive(Clone, PartialEq, Eq)]
pub struct ReportMenu {
    name: String,
    version: String,
    variant: String,
    root: u32,
    nodes: BTreeMap<u32, MenuNode>,
}

impl fmt::Debug for ReportMenu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReportMenu({} nodes)", self.nodes.len())
    }
}

/// Discord's acknowledgement of a submission.
#[derive(Clone, PartialEq, Eq)]
pub struct ReportReceipt {
    /// Discord's reference for the report, when it sent one.
    pub id: Option<String>,
}

impl fmt::Debug for ReportReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReportReceipt")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportError {
    /// The path is not a walk of this menu ending at a submittable node.
    InvalidPath,
    /// The menu is not for this target.
    WrongMenu,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportRequestError {
    Invalid(ReportError),
    Rest(RestError),
}

impl From<RestError> for ReportRequestError {
    fn from(error: RestError) -> Self {
        Self::Rest(error)
    }
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn token(value: &serde_json::Value) -> Option<String> {
    let text = value.as_str()?;
    (!text.is_empty()
        && text.chars().count() <= MAX_TOKEN_CHARS
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    .then(|| text.to_owned())
}

fn node_id(value: &serde_json::Value) -> Option<u32> {
    match value {
        serde_json::Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        serde_json::Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// A choice is a two-element array of a label and a node ID, in either order.
fn choice(value: &serde_json::Value) -> Option<(String, u32)> {
    let pair = value.as_array().filter(|pair| pair.len() == 2)?;
    let (label, id) = match (pair[0].as_str(), pair[1].as_str()) {
        (Some(label), None) => (label, node_id(&pair[1])?),
        (None, Some(label)) => (label, node_id(&pair[0])?),
        _ => return None,
    };
    Some((clip(label, MAX_LABEL_CHARS), id))
}

impl ReportMenu {
    /// Lenient about extra fields; strict about bounds and about names that
    /// would be written back into the submission.
    pub fn parse(body: &[u8], target: ReportTarget) -> Result<Self, ReportRequestError> {
        let value: serde_json::Value =
            serde_json::from_slice(body).map_err(|_| RestError::InvalidJson)?;
        Self::from_value(&value, target)
    }

    fn from_value(
        value: &serde_json::Value,
        target: ReportTarget,
    ) -> Result<Self, ReportRequestError> {
        let invalid = || ReportRequestError::Rest(RestError::InvalidJson);
        let name = value.get("name").and_then(token).ok_or_else(invalid)?;
        if name != target.name() {
            return Err(ReportRequestError::Invalid(ReportError::WrongMenu));
        }
        let version = value.get("version").and_then(token).ok_or_else(invalid)?;
        let variant = value.get("variant").and_then(token).ok_or_else(invalid)?;
        let root = value
            .get("root_node_id")
            .and_then(node_id)
            .ok_or_else(invalid)?;
        let raw = value
            .get("nodes")
            .and_then(serde_json::Value::as_object)
            .filter(|nodes| nodes.len() <= MAX_MENU_NODES)
            .ok_or_else(invalid)?;
        let ends: Vec<u32> = ["success_node_id", "fail_node_id"]
            .iter()
            .filter_map(|key| value.get(key).and_then(node_id))
            .collect();
        let mut nodes = BTreeMap::new();
        for (key, node) in raw {
            let id = key.parse::<u32>().map_err(|_| invalid())?;
            let text = |field: &str, limit| {
                node.get(field)
                    .and_then(serde_json::Value::as_str)
                    .map(|text| clip(text, limit))
                    .unwrap_or_default()
            };
            let choices: Vec<_> = node
                .get("children")
                .and_then(serde_json::Value::as_array)
                .map(|list| list.iter().filter_map(choice).take(MAX_CHOICES).collect())
                .unwrap_or_default();
            let has_elements = node
                .get("elements")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|list| !list.is_empty());
            let cancel = node
                .get("button")
                .and_then(|b| b.get("type"))
                .and_then(serde_json::Value::as_str)
                == Some("cancel");
            let kind = if !choices.is_empty() {
                NodeKind::Choice
            } else if has_elements || cancel || ends.contains(&id) {
                NodeKind::Unsupported
            } else {
                NodeKind::Submit
            };
            nodes.insert(
                id,
                MenuNode {
                    header: text("header", MAX_TEXT_CHARS),
                    subheader: text("subheader", MAX_TEXT_CHARS),
                    choices,
                    kind,
                },
            );
        }
        if !nodes.contains_key(&root) {
            return Err(invalid());
        }
        // A choice to a node that is not there is not offered.
        let known: Vec<u32> = nodes.keys().copied().collect();
        for node in nodes.values_mut() {
            node.choices.retain(|(_, id)| known.contains(id));
            if node.kind == NodeKind::Choice && node.choices.is_empty() {
                node.kind = NodeKind::Unsupported;
            }
        }
        Ok(Self {
            name,
            version,
            variant,
            root,
            nodes,
        })
    }

    pub fn root(&self) -> u32 {
        self.root
    }

    pub fn node(&self, id: u32) -> Option<&MenuNode> {
        self.nodes.get(&id)
    }

    /// The nodes a path visits after the root, or why it is not a walk that
    /// ends where a report can be submitted.
    pub fn walk(&self, path: &[u32]) -> Result<Vec<&MenuNode>, ReportError> {
        if path.is_empty() || path.len() > MAX_DEPTH {
            return Err(ReportError::InvalidPath);
        }
        let mut at = self.node(self.root).ok_or(ReportError::InvalidPath)?;
        let mut visited = Vec::with_capacity(path.len());
        for id in path {
            if !at.choices.iter().any(|(_, child)| child == id) {
                return Err(ReportError::InvalidPath);
            }
            at = self.node(*id).ok_or(ReportError::InvalidPath)?;
            visited.push(at);
        }
        if at.kind == NodeKind::Submit {
            Ok(visited)
        } else {
            Err(ReportError::InvalidPath)
        }
    }

    /// The one submission body for a validated path. The target's content is
    /// never sent: Discord resolves the IDs itself.
    pub fn submission(
        &self,
        target: ReportTarget,
        path: &[u32],
    ) -> Result<serde_json::Value, ReportError> {
        if target.name() != self.name {
            return Err(ReportError::WrongMenu);
        }
        self.walk(path)?;
        let mut body = serde_json::json!({
            "version": self.version,
            "variant": self.variant,
            "language": "en",
            "breadcrumbs": path,
            "elements": {},
            "name": self.name,
        });
        let ids = body.as_object_mut().expect("object literal");
        match target {
            ReportTarget::Message { channel, message } => {
                ids.insert("channel_id".into(), channel.to_string().into());
                ids.insert("message_id".into(), message.to_string().into());
            }
            ReportTarget::User(user) => {
                ids.insert("user_id".into(), user.to_string().into());
            }
            ReportTarget::Server(guild) => {
                ids.insert("guild_id".into(), guild.to_string().into());
            }
        }
        Ok(body)
    }
}

/// A 2xx is the receipt; an unreadable body only means no reference.
fn parse_receipt(body: &[u8]) -> ReportReceipt {
    let id = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|value| match value.get("report_id")? {
            serde_json::Value::String(id) => Some(clip(id, MAX_RECEIPT_CHARS)),
            serde_json::Value::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .filter(|id| !id.is_empty());
    ReportReceipt { id }
}

impl<C: Clock> RestClient<C> {
    /// Reads the menu for a target (`GET /reporting/menu/{name}`). A read:
    /// nothing is reported by it.
    pub async fn report_menu(
        &self,
        target: ReportTarget,
    ) -> Result<ReportMenu, ReportRequestError> {
        let route = Route::new(Method::GET, &format!("/reporting/menu/{}", target.name()))?;
        let response = self
            .execute(RestRequest::new(route, Priority::UserRead))
            .await?;
        ReportMenu::parse(response.body(), target)
    }

    /// Submits one report (`POST /reporting/{name}`). Exactly one request and
    /// never retried: a failure that is not a definite refusal may still have
    /// been received, so only the person can decide to report again.
    pub async fn submit_report(
        &self,
        menu: &ReportMenu,
        target: ReportTarget,
        path: &[u32],
    ) -> Result<ReportReceipt, ReportRequestError> {
        let body = menu
            .submission(target, path)
            .map_err(ReportRequestError::Invalid)?;
        let request = RestRequest::new(
            Route::new(Method::POST, &format!("/reporting/{}", target.name()))?,
            Priority::UserWrite,
        )
        .json(&body)?;
        let response = self.execute(request).await?;
        Ok(parse_receipt(response.body()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../fixtures/rest/report-menu.json");

    fn fixture(key: &str) -> serde_json::Value {
        serde_json::from_str::<serde_json::Value>(FIXTURE).unwrap()[key].clone()
    }

    const MESSAGE: ReportTarget = ReportTarget::Message {
        channel: Snowflake(11),
        message: Snowflake(22),
    };

    fn menu() -> ReportMenu {
        ReportMenu::from_value(&fixture("message"), MESSAGE).unwrap()
    }

    #[test]
    fn the_menu_is_rendered_from_data_with_either_choice_order() {
        let menu = menu();
        let root = menu.node(menu.root()).unwrap();
        assert_eq!(root.kind, NodeKind::Choice);
        assert_eq!(root.choices.len(), 3);
        // [label, id] and [id, label] both read.
        assert_eq!(menu.node(3).unwrap().choices[0], ("Harassment".into(), 4));
        assert_eq!(menu.node(1).unwrap().kind, NodeKind::Submit);
        // Input this client cannot collect, a cancel button and the end
        // nodes are never a way to submit.
        assert_eq!(menu.node(2).unwrap().kind, NodeKind::Unsupported);
        assert_eq!(menu.node(90).unwrap().kind, NodeKind::Unsupported);
        assert_eq!(menu.node(91).unwrap().kind, NodeKind::Unsupported);
    }

    #[test]
    fn only_a_real_walk_to_a_submit_node_builds_a_body() {
        let menu = menu();
        let body = menu.submission(MESSAGE, &[1]).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "version": "1.0", "variant": "4", "language": "en",
                "breadcrumbs": [1], "elements": {}, "name": "message",
                "channel_id": "11", "message_id": "22"
            })
        );
        assert_eq!(
            menu.submission(MESSAGE, &[3, 4]).unwrap()["breadcrumbs"],
            serde_json::json!([3, 4])
        );
        for bad in [
            &[][..],
            &[3][..],
            &[2][..],
            &[4][..],
            &[9][..],
            &[3, 90][..],
            &[1, 1][..],
        ] {
            assert_eq!(
                menu.submission(MESSAGE, bad),
                Err(ReportError::InvalidPath),
                "{bad:?}"
            );
        }
        assert_eq!(
            menu.submission(ReportTarget::User(Snowflake(5)), &[1]),
            Err(ReportError::WrongMenu)
        );
    }

    #[test]
    fn user_and_server_bodies_name_only_their_id() {
        let mut value = fixture("message");
        value["name"] = "user".into();
        let user = ReportTarget::User(Snowflake(5));
        let body = ReportMenu::from_value(&value, user)
            .unwrap()
            .submission(user, &[1])
            .unwrap();
        assert_eq!(body["user_id"], "5");
        assert!(body.get("message_id").is_none());
        value["name"] = "guild".into();
        let server = ReportTarget::Server(Snowflake(6));
        let body = ReportMenu::from_value(&value, server)
            .unwrap()
            .submission(server, &[1])
            .unwrap();
        assert_eq!(body["guild_id"], "6");
    }

    #[test]
    fn hostile_or_mismatched_menus_are_refused() {
        for key in ["hostile", "no_root"] {
            assert_eq!(
                ReportMenu::from_value(&fixture(key), MESSAGE),
                Err(ReportRequestError::Rest(RestError::InvalidJson)),
                "{key}"
            );
        }
        assert_eq!(
            ReportMenu::from_value(&fixture("wrong_name"), MESSAGE),
            Err(ReportRequestError::Invalid(ReportError::WrongMenu))
        );
        assert!(ReportMenu::parse(b"[]", MESSAGE).is_err());
        assert!(ReportMenu::parse(b"nonsense", MESSAGE).is_err());
    }

    #[test]
    fn menus_are_bounded() {
        let mut value = fixture("message");
        let nodes = value["nodes"].as_object_mut().unwrap();
        for id in 100..100 + MAX_MENU_NODES {
            nodes.insert(id.to_string(), serde_json::json!({"id": id}));
        }
        assert!(ReportMenu::from_value(&value, MESSAGE).is_err());
        // Too many choices and long labels are cut, not trusted.
        let many: Vec<_> = (0..100)
            .map(|_| serde_json::json!(["x".repeat(900), 1]))
            .collect();
        let value = serde_json::json!({
            "name": "message", "version": "1", "variant": "1", "root_node_id": 0,
            "nodes": {"0": {"children": many, "header": "h".repeat(9000)}, "1": {}}
        });
        let menu = ReportMenu::from_value(&value, MESSAGE).unwrap();
        let root = menu.node(0).unwrap();
        assert_eq!(root.choices.len(), MAX_CHOICES);
        assert_eq!(root.choices[0].0.chars().count(), MAX_LABEL_CHARS);
        assert_eq!(root.header.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn a_choice_to_a_missing_node_is_not_offered() {
        let value = serde_json::json!({
            "name": "message", "version": "1", "variant": "1", "root_node_id": 0,
            "nodes": {"0": {"children": [["Gone", 7]]}}
        });
        let menu = ReportMenu::from_value(&value, MESSAGE).unwrap();
        assert_eq!(menu.node(0).unwrap().kind, NodeKind::Unsupported);
    }

    #[test]
    fn a_2xx_is_a_receipt_with_or_without_a_reference() {
        assert_eq!(
            parse_receipt(br#"{"report_id":"123"}"#).id.as_deref(),
            Some("123")
        );
        assert_eq!(parse_receipt(b"").id, None);
        assert_eq!(parse_receipt(b"{}").id, None);
        assert_eq!(parse_receipt(b"nonsense").id, None);
    }

    #[test]
    fn debug_output_names_no_target_or_text() {
        let text = format!("{:?} {:?} {:?}", MESSAGE, menu(), menu().node(1).unwrap());
        assert!(!text.contains("11") && !text.contains("Spam"), "{text}");
    }
}
