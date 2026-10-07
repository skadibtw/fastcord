//! Fixture-driven tests for wire decoding and permission computation.

use fastcord_model::{
    Channel, GuildScope, MemberScope, Message, MessageUpdate, Permissions, Role, Snowflake,
    channel_permissions,
};
use serde::Deserialize;

fn fixture(name: &str) -> String {
    let path = format!("{}/../../fixtures/model/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn named(name: &str) -> Permissions {
    match name {
        "ADMINISTRATOR" => Permissions::ADMINISTRATOR,
        "ADD_REACTIONS" => Permissions::ADD_REACTIONS,
        "STREAM" => Permissions::STREAM,
        "VIEW_CHANNEL" => Permissions::VIEW_CHANNEL,
        "SEND_MESSAGES" => Permissions::SEND_MESSAGES,
        "EMBED_LINKS" => Permissions::EMBED_LINKS,
        "ATTACH_FILES" => Permissions::ATTACH_FILES,
        "READ_MESSAGE_HISTORY" => Permissions::READ_MESSAGE_HISTORY,
        "MENTION_EVERYONE" => Permissions::MENTION_EVERYONE,
        "CONNECT" => Permissions::CONNECT,
        "SPEAK" => Permissions::SPEAK,
        other => panic!("unknown permission name in fixture: {other}"),
    }
}

#[derive(Deserialize)]
struct Case {
    name: String,
    guild: GuildFixture,
    member: MemberFixture,
    channel: Channel,
    allow: Vec<String>,
    deny: Vec<String>,
}

#[derive(Deserialize)]
struct GuildFixture {
    id: Snowflake,
    owner_id: Snowflake,
    roles: Vec<Role>,
}

#[derive(Deserialize)]
struct MemberFixture {
    user_id: Snowflake,
    roles: Vec<Snowflake>,
    #[serde(default)]
    timed_out: bool,
}

#[test]
fn permission_fixtures() {
    let cases: Vec<Case> = serde_json::from_str(&fixture("permissions.json")).unwrap();
    assert!(cases.len() >= 7);
    for case in cases {
        let perms = channel_permissions(
            GuildScope {
                id: case.guild.id,
                owner_id: case.guild.owner_id,
                roles: &case.guild.roles,
            },
            MemberScope {
                user_id: case.member.user_id,
                role_ids: &case.member.roles,
                timed_out: case.member.timed_out,
            },
            &case.channel.permission_overwrites,
            case.channel.kind.is_text_like(),
        );
        for p in &case.allow {
            assert!(perms.contains(named(p)), "{}: expected {p}", case.name);
        }
        for p in &case.deny {
            assert!(!perms.contains(named(p)), "{}: unexpected {p}", case.name);
        }
    }
}

#[test]
fn message_create_decodes_and_ignores_unknown_fields() {
    let msg: Message = serde_json::from_str(&fixture("message_create.json")).unwrap();
    assert_eq!(msg.author.display_name(), "Alt Account");
    assert_eq!(msg.kind, 19);
    assert_eq!(msg.attachments[0].width, Some(640));
    assert_eq!(msg.reactions[0].emoji.name.as_deref(), Some("\u{1f44d}"));
    assert!(msg.reactions[0].me);
    assert_eq!(
        msg.message_reference.unwrap().message_id,
        Some(Snowflake(1_289_999_999_999_999_999))
    );
}

#[test]
fn partial_update_keeps_omitted_fields() {
    let mut msg: Message = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let before = msg.clone();
    let update: MessageUpdate =
        serde_json::from_str(&fixture("message_update_partial.json")).unwrap();
    msg.apply(update);
    assert_eq!(
        msg, before,
        "embed-only update must not clear content/attachments"
    );
}

#[test]
fn edit_update_replaces_content_and_keeps_attachments() {
    let mut msg: Message = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let update: MessageUpdate = serde_json::from_str(&fixture("message_update_edit.json")).unwrap();
    msg.apply(update);
    assert_eq!(msg.content, "hello (edited)");
    assert!(msg.edited_timestamp.is_some());
    assert_eq!(msg.attachments.len(), 1);
}

#[test]
fn explicit_null_clears_edited_timestamp() {
    let mut msg: Message = serde_json::from_str(&fixture("message_create.json")).unwrap();
    msg.edited_timestamp = Some("x".into());
    let update: MessageUpdate = serde_json::from_str(
        r#"{"id":"1290000000000000001","channel_id":"500","edited_timestamp":null}"#,
    )
    .unwrap();
    msg.apply(update);
    assert_eq!(msg.edited_timestamp, None);
}

#[test]
fn alters_agrees_with_what_apply_would_change() {
    let msg: Message = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let id = "1290000000000000001";
    let cases = [
        // An embed-only update carries nothing the client models.
        (fixture("message_update_partial.json"), false),
        (fixture("message_update_edit.json"), true),
        // Repeating the current values changes nothing.
        (
            format!(
                r#"{{"id":"{id}","channel_id":"500","content":"hello <:wave:777>","edited_timestamp":null,"flags":0,"pinned":false}}"#
            ),
            false,
        ),
        (
            format!(
                r#"{{"id":"{id}","channel_id":"500","edited_timestamp":"2026-10-07T12:05:00+00:00"}}"#
            ),
            true,
        ),
        (
            format!(r#"{{"id":"{id}","channel_id":"500","content":""}}"#),
            true,
        ),
        (
            format!(r#"{{"id":"{id}","channel_id":"500","attachments":[]}}"#),
            true,
        ),
        (
            format!(r#"{{"id":"{id}","channel_id":"500","pinned":true}}"#),
            true,
        ),
        (
            format!(r#"{{"id":"{id}","channel_id":"500","flags":64}}"#),
            true,
        ),
    ];
    for (raw, expected) in cases {
        let update: MessageUpdate = serde_json::from_str(&raw).unwrap();
        let mut applied = msg.clone();
        applied.apply(update.clone());
        assert_eq!(update.alters(&msg), expected, "{raw}");
        assert_eq!(
            update.alters(&msg),
            applied != msg,
            "alters must agree with apply: {raw}"
        );
    }
}

#[test]
fn has_fields_ignores_unmodeled_payload_but_counts_an_explicit_null() {
    let embed_only: MessageUpdate =
        serde_json::from_str(&fixture("message_update_partial.json")).unwrap();
    assert!(!embed_only.has_fields());
    let ids_only: MessageUpdate = serde_json::from_str(r#"{"id":"1","channel_id":"2"}"#).unwrap();
    assert!(!ids_only.has_fields());
    for field in [
        r#""content":"x""#,
        r#""edited_timestamp":null"#,
        r#""flags":0"#,
        r#""pinned":false"#,
        r#""attachments":[]"#,
    ] {
        let raw = format!(r#"{{"id":"1","channel_id":"2",{field}}}"#);
        let update: MessageUpdate = serde_json::from_str(&raw).unwrap();
        assert!(update.has_fields(), "{field}");
    }
}

#[test]
fn debug_output_names_ids_but_never_content_or_signed_urls() {
    let msg: Message = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let update: MessageUpdate = serde_json::from_str(&fixture("message_update_edit.json")).unwrap();
    let shown = format!("{msg:?} {update:?} {:?}", msg.attachments);
    assert!(shown.contains("1290000000000000001"));
    for secret in ["hello", "edited", "SIGNED", "cat.png", "Alt Account"] {
        assert!(!shown.contains(secret), "{secret}");
    }
}

#[test]
fn unknown_channel_type_is_preserved() {
    let ch: Channel = serde_json::from_str(r#"{"id":"1","type":99}"#).unwrap();
    assert_eq!(ch.kind.to_u8(), 99);
    assert!(!ch.kind.is_text_like());
    assert_eq!(serde_json::to_value(ch.kind).unwrap(), 99);
}
