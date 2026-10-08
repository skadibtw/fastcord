//! Editing and deleting the account's own messages (SPEC §2.1, §5.2):
//! `PATCH /channels/{channel}/messages/{message}` and
//! `DELETE /channels/{channel}/messages/{message}`.
//!
//! Only the user's own ordinary messages qualify ([`is_own_message`]); deleting
//! other people's messages is moderation, which is not an MVP feature. An edit
//! sends `content` and nothing else. Every field Discord's edit route accepts
//! but the request omits stays as it is, which is what keeps a text edit from
//! removing attachments: an `attachments` list in the body would replace the
//! message's attachments with exactly that list.
//!
//! Both requests are idempotent, so unlike a send nothing here can create a
//! duplicate; the transport still never repeats one by itself. What the REST
//! answer reports and what the Gateway reports can arrive in either order;
//! [`edit_response_update`] turns an edit's answer into the same partial update
//! the Gateway would deliver, unless the cached copy is already newer.

use std::cmp::Ordering;
use std::fmt;

use fastcord_model::{Message, MessageUpdate, Snowflake};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{
    Clock, ComposeError, MAX_CONTENT_CHARS, Method, Priority, RestClient, RestError, RestRequest,
    Route,
};

/// Message types a user writes themselves: `DEFAULT` and `REPLY`. System
/// messages that name the user as author (joins, pins, boosts, calls) are not
/// offered for editing or deletion.
const USER_KINDS: [u8; 2] = [0, 19];

/// Whether `message` is one the account `user` wrote and may therefore edit
/// and delete. The server stays authoritative (it may still refuse, for
/// example while the account is timed out); this decides what the UI offers
/// and what the worker is willing to request.
pub fn is_own_message(message: &Message, user: Snowflake) -> bool {
    message.author.id == user && USER_KINDS.contains(&message.kind)
}

/// New text for an existing message. `Debug` never prints the text.
#[derive(Clone, PartialEq, Eq)]
pub struct EditedMessage {
    content: String,
}

impl EditedMessage {
    /// Surrounding whitespace is trimmed, as for a send. Empty text is only
    /// allowed when the message keeps attachments: Discord refuses a message
    /// with nothing in it, and removing the text is then a delete.
    pub fn new(content: &str, has_attachments: bool) -> Result<Self, ComposeError> {
        let content = content.trim();
        if content.is_empty() && !has_attachments {
            return Err(ComposeError::Empty);
        }
        if content.chars().count() > MAX_CONTENT_CHARS {
            return Err(ComposeError::TooLong);
        }
        Ok(Self {
            content: content.to_owned(),
        })
    }

    /// The text exactly as it is sent.
    pub fn content(&self) -> &str {
        &self.content
    }

    fn body(&self) -> EditBody<'_> {
        EditBody {
            content: &self.content,
        }
    }
}

impl fmt::Debug for EditedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditedMessage")
            .field("content_chars", &self.content.chars().count())
            .finish()
    }
}

/// The whole edit body. Deliberately no `attachments`, `flags`, `embeds`, or
/// `allowed_mentions`: omitted means unchanged.
#[derive(Serialize)]
struct EditBody<'a> {
    content: &'a str,
}

fn message_route(
    method: Method,
    channel: Snowflake,
    message: Snowflake,
) -> Result<Route, RestError> {
    Route::new(method, &format!("/channels/{channel}/messages/{message}"))
}

/// Decodes an edit's answer. A success status whose body is not the edited
/// message cannot be applied; the caller learns the outcome from the Gateway.
pub(crate) fn parse_edited(
    body: &[u8],
    channel: Snowflake,
    id: Snowflake,
) -> Result<Message, RestError> {
    serde_json::from_slice::<Message>(body)
        .ok()
        .filter(|message| message.channel_id == channel && message.id == id)
        .ok_or(RestError::InvalidJson)
}

/// Compares two `edited_timestamp`s. `None` when either cannot be read.
fn edited_order(held: &str, answer: &str) -> Option<Ordering> {
    let held = OffsetDateTime::parse(held, &Rfc3339).ok()?;
    let answer = OffsetDateTime::parse(answer, &Rfc3339).ok()?;
    Some(held.cmp(&answer))
}

/// The partial update that applies an edit's REST answer to the cached copy
/// `held`, or `None` when `held` already shows a later edit (another client
/// edited the message again and the Gateway reported it before this answer
/// arrived). The update carries every field the answer has, so it replaces
/// attachments only with the list Discord itself reports. A message that is not
/// cached still gets its update: the store then merely notes it for any history
/// page in flight, and never recreates a message from it.
pub fn edit_response_update(held: Option<&Message>, answer: Message) -> Option<MessageUpdate> {
    let held_is_newer = held.is_some_and(|held| {
        match (
            held.edited_timestamp.as_deref(),
            answer.edited_timestamp.as_deref(),
        ) {
            (None, _) => false,
            // Discord reports an edited message's time; an answer without one
            // cannot be newer than a copy that has it.
            (Some(_), None) => true,
            (Some(held), Some(answer)) => edited_order(held, answer) == Some(Ordering::Greater),
        }
    });
    if held_is_newer {
        return None;
    }
    Some(MessageUpdate {
        id: answer.id,
        channel_id: answer.channel_id,
        content: Some(answer.content),
        edited_timestamp: Some(answer.edited_timestamp),
        flags: Some(answer.flags),
        pinned: Some(answer.pinned),
        attachments: Some(answer.attachments),
    })
}

impl<C: Clock> RestClient<C> {
    /// Replaces the text of one of the user's messages and returns the message
    /// as Discord now has it. Sent once; a failure is returned to the caller,
    /// and since the same edit applied twice changes nothing, the user may
    /// retry it safely.
    pub async fn edit_message(
        &self,
        channel: Snowflake,
        message: Snowflake,
        edit: &EditedMessage,
    ) -> Result<Message, RestError> {
        let request = RestRequest::new(
            message_route(Method::PATCH, channel, message)?,
            Priority::UserWrite,
        )
        .json(&edit.body())?;
        let response = self.execute(request).await?;
        parse_edited(response.body(), channel, message)
    }

    /// Deletes one of the user's messages. `ResourceGone` (404) means the
    /// message no longer exists, which callers treat as deleted.
    pub async fn delete_message(
        &self,
        channel: Snowflake,
        message: Snowflake,
    ) -> Result<(), RestError> {
        let request = RestRequest::new(
            message_route(Method::DELETE, channel, message)?,
            Priority::UserWrite,
        );
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserToken;

    const REQUEST: &str = include_str!("../../../fixtures/rest/message-edit-request.json");
    const RESPONSE: &str = include_str!("../../../fixtures/rest/message-edit-response.json");
    const CHANNEL: Snowflake = Snowflake(500);
    const ID: Snowflake = Snowflake(1_290_000_000_000_000_099);

    fn answer() -> Message {
        parse_edited(RESPONSE.as_bytes(), CHANNEL, ID).unwrap()
    }

    #[test]
    fn the_edit_body_is_the_text_alone_so_attachments_stay() {
        let edit =
            EditedMessage::new("  hello again from fastcord \u{e9} (edited)\n", false).unwrap();
        let sent = serde_json::to_value(edit.body()).unwrap();
        let recorded: serde_json::Value = serde_json::from_str(REQUEST).unwrap();
        assert_eq!(sent, recorded);
        // Any of these would change the message beyond its text; `attachments`
        // in particular would replace its attachments with the list sent.
        let object = sent.as_object().unwrap();
        assert_eq!(object.len(), 1);
        for absent in [
            "attachments",
            "flags",
            "embeds",
            "allowed_mentions",
            "nonce",
        ] {
            assert!(!object.contains_key(absent), "{absent}");
        }
    }

    #[test]
    fn edits_are_trimmed_bounded_and_may_be_empty_only_with_attachments() {
        assert_eq!(EditedMessage::new(" \n ", false), Err(ComposeError::Empty));
        let kept = EditedMessage::new(" \n ", true).unwrap();
        assert_eq!(kept.content(), "", "the attachments remain the message");
        let at_limit = "\u{e9}".repeat(MAX_CONTENT_CHARS);
        assert!(EditedMessage::new(&at_limit, false).is_ok());
        assert_eq!(
            EditedMessage::new(&format!("{at_limit}x"), true),
            Err(ComposeError::TooLong)
        );
        let edit = EditedMessage::new("  a \n b  ", false).unwrap();
        assert_eq!(edit.content(), "a \n b");
        let shown = format!("{edit:?}");
        assert!(!shown.contains(" b") && shown.contains("content_chars: 5"));
    }

    #[test]
    fn only_the_users_own_ordinary_messages_can_be_changed() {
        let mut message = answer();
        let me = message.author.id;
        assert!(is_own_message(&message, me));
        assert!(!is_own_message(&message, Snowflake(3)), "someone else's");
        message.kind = 19;
        assert!(is_own_message(&message, me), "a reply");
        // System messages that name the user as author.
        for kind in [1, 3, 6, 7, 8, 18, 21, 46] {
            message.kind = kind;
            assert!(!is_own_message(&message, me), "type {kind}");
        }
    }

    #[test]
    fn routes_name_the_message_and_keep_the_channel_as_major_parameter() {
        let edit = message_route(Method::PATCH, CHANNEL, ID).unwrap();
        let delete = message_route(Method::DELETE, CHANNEL, ID).unwrap();
        assert_eq!(
            edit.url.as_str(),
            "https://discord.com/api/v10/channels/500/messages/1290000000000000099"
        );
        assert_eq!(edit.url, delete.url);
        assert_eq!(edit.key().method(), &Method::PATCH);
        assert_eq!(delete.key().method(), &Method::DELETE);
        for route in [&edit, &delete] {
            assert_eq!(
                route.key().normalized(),
                "/channels/{channel_id}/messages/{id}"
            );
            assert_eq!(route.key().major().channel_id(), Some(CHANNEL));
        }
        // Both are user writes whatever priority is asked for.
        let shown = format!("{:?}", RestRequest::new(edit, Priority::SpeculativeRead));
        assert!(shown.contains("UserWrite") && !shown.contains("1290000000000000099"));
    }

    #[test]
    fn the_answer_must_be_the_edited_message() {
        let message = answer();
        assert_eq!(message.content, "hello again from fastcord \u{e9} (edited)");
        assert_eq!(message.attachments.len(), 1);
        assert_eq!(
            message.edited_timestamp.as_deref(),
            Some("2026-10-08T09:05:00.123000+00:00")
        );
        for (body, channel, id) in [
            (RESPONSE.as_bytes(), Snowflake(501), ID),
            (RESPONSE.as_bytes(), CHANNEL, Snowflake(1)),
            (b"{}".as_slice(), CHANNEL, ID),
            (b"".as_slice(), CHANNEL, ID),
        ] {
            assert_eq!(parse_edited(body, channel, id), Err(RestError::InvalidJson));
        }
    }

    #[test]
    fn an_answer_applies_unless_the_cached_copy_shows_a_later_edit() {
        let answer = answer();
        let mut held = answer.clone();
        held.content = "before".to_owned();
        held.edited_timestamp = None;
        held.attachments.clear();
        // Never edited before, or edited earlier: the answer wins, field by field.
        let update = edit_response_update(Some(&held), answer.clone()).unwrap();
        assert!(update.alters(&held));
        let mut applied = held.clone();
        applied.apply(update);
        assert_eq!(applied, answer, "attachments come from Discord's answer");
        held.edited_timestamp = Some("2026-10-08T09:04:59.999999+00:00".to_owned());
        assert!(edit_response_update(Some(&held), answer.clone()).is_some());
        // The same edit, already reported by the Gateway: applying changes nothing.
        let update = edit_response_update(Some(&answer), answer.clone()).unwrap();
        assert!(!update.alters(&answer));
        // Edited again after this answer. Times are compared as instants,
        // whatever their offsets: 11:05:00.1 at +02:00 is before the answer's
        // 09:05:00.123 UTC, 11:05:01 at +02:00 after it.
        held.edited_timestamp = Some("2026-10-08T11:05:00.100000+02:00".to_owned());
        assert!(edit_response_update(Some(&held), answer.clone()).is_some());
        held.edited_timestamp = Some("2026-10-08T11:05:01.000000+02:00".to_owned());
        assert!(edit_response_update(Some(&held), answer.clone()).is_none());
        // An answer claiming no edit cannot be newer than an edited copy.
        let mut unedited = answer.clone();
        unedited.edited_timestamp = None;
        assert!(edit_response_update(Some(&held), unedited).is_none());
        // Unreadable times do not block Discord's own answer.
        held.edited_timestamp = Some("yesterday".to_owned());
        assert!(edit_response_update(Some(&held), answer.clone()).is_some());
        // Nothing cached: the update is still produced, for in-flight pages.
        assert!(edit_response_update(None, answer).is_some());
    }

    #[tokio::test]
    async fn a_stopped_account_never_edits_or_deletes() {
        let client = RestClient::new(UserToken::new("offline-test-credential".to_owned())).unwrap();
        client.stop_authenticated_work();
        let edit = EditedMessage::new("text", false).unwrap();
        assert_eq!(
            client.edit_message(CHANNEL, ID, &edit).await,
            Err(RestError::AuthenticationRequired)
        );
        assert_eq!(
            client.delete_message(CHANNEL, ID).await,
            Err(RestError::AuthenticationRequired)
        );
    }
}
