//! Fixture-driven tests for wire decoding and permission computation.

use fastcord_model::{
    Channel, GuildScope, MemberScope, Message, MessageUpdate, Permissions, REPLY_PREVIEW_CHARS,
    Referenced, Role, Snowflake, channel_permissions,
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
fn nonce_is_kept_when_it_is_a_short_string_or_integer_and_never_fails_the_message() {
    let base: serde_json::Value = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let decode = |nonce: Option<serde_json::Value>| -> Message {
        let mut value = base.clone();
        if let Some(nonce) = nonce {
            value["nonce"] = nonce;
        }
        serde_json::from_value(value).expect("a hostile nonce must not drop the message")
    };
    assert_eq!(decode(None).nonce, None, "absent");
    assert_eq!(decode(Some(serde_json::Value::Null)).nonce, None, "null");
    assert_eq!(
        decode(Some("1290000000000000000".into())).nonce.as_deref(),
        Some("1290000000000000000"),
        "the web client's snowflake-valued string"
    );
    assert_eq!(
        decode(Some(serde_json::json!(1_290_000_000_000_000_000_u64)))
            .nonce
            .as_deref(),
        Some("1290000000000000000"),
        "integers (bots) become their decimal text"
    );
    assert_eq!(
        decode(Some("n".repeat(25).into())).nonce.as_deref(),
        Some("n".repeat(25).as_str()),
        "25 characters is the limit"
    );
    for hostile in [
        serde_json::json!("n".repeat(26)),
        serde_json::json!(1.5),
        serde_json::json!(true),
        serde_json::json!([1, 2, {"a": "b"}]),
        serde_json::json!({"nested": {"deep": [1]}}),
    ] {
        assert_eq!(decode(Some(hostile)).nonce, None);
    }
    // Serializing never invents one.
    let plain = serde_json::to_value(decode(None)).unwrap();
    assert!(plain.get("nonce").is_none());
}

#[test]
fn referenced_message_tells_unknown_deleted_and_present_apart() {
    let base: serde_json::Value = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let decode = |referenced: Option<serde_json::Value>| -> Message {
        let mut value = base.clone();
        if let Some(referenced) = referenced {
            value["referenced_message"] = referenced;
        }
        serde_json::from_value(value).unwrap()
    };
    // Absent: Discord did not look the message up.
    let unknown = decode(None);
    assert_eq!(unknown.referenced_message, Referenced::Unknown);
    assert_eq!(
        unknown.replied_to(),
        Some(Snowflake(1_289_999_999_999_999_999))
    );
    // `null`: it was deleted.
    assert_eq!(
        decode(Some(serde_json::Value::Null)).referenced_message,
        Referenced::Deleted
    );
    // An object: only the author, the ID, and the start of the text are kept;
    // its own nested reference and unknown fields are skipped.
    let long = "\u{e9}".repeat(REPLY_PREVIEW_CHARS + 50);
    let present = decode(Some(serde_json::json!({
        "id": "1289999999999999999",
        "type": 19,
        "channel_id": "500",
        "content": long,
        "author": {"id": "3", "username": "other_fixture", "global_name": "Other", "discriminator": "0"},
        "attachments": [],
        "embeds": [{"type": "rich"}],
        "timestamp": "2026-10-07T11:59:00.000000+00:00",
        "message_reference": {"message_id": "1289999999999999998"},
        "referenced_message": {"id": "1289999999999999998", "content": "deeper", "author": {"id": "4", "username": "x"}}
    })));
    let Referenced::Message(preview) = &present.referenced_message else {
        panic!("expected a preview");
    };
    assert_eq!(preview.id, Snowflake(1_289_999_999_999_999_999));
    assert_eq!(preview.author.display_name(), "Other");
    assert_eq!(
        preview.content.chars().count(),
        REPLY_PREVIEW_CHARS,
        "cut to the preview length, in characters"
    );
    assert!(preview.content.chars().all(|c| c == '\u{e9}'));
    // A missing text is empty rather than a decoding failure.
    let textless = decode(Some(
        serde_json::json!({"id": "9", "author": {"id": "3", "username": "u"}}),
    ));
    assert!(matches!(&textless.referenced_message, Referenced::Message(p) if p.content.is_empty()));
    // The three states survive a round trip in their wire form.
    for message in [&unknown, &decode(Some(serde_json::Value::Null)), &present] {
        let wire = serde_json::to_value(message).unwrap();
        assert_eq!(
            wire.get("referenced_message").is_some(),
            !message.referenced_message.is_unknown()
        );
        let again: Message = serde_json::from_value(wire).unwrap();
        assert_eq!(&again, message);
    }
    // The quoted text never reaches Debug output.
    assert!(!format!("{present:?} {:?}", present.referenced_message).contains('\u{e9}'));
}

#[test]
fn only_same_channel_replies_have_a_reply_target() {
    let base: serde_json::Value = serde_json::from_str(&fixture("message_create.json")).unwrap();
    let with = |kind: u8, reference: serde_json::Value| -> Option<Snowflake> {
        let mut value = base.clone();
        value["type"] = kind.into();
        value["message_reference"] = reference;
        serde_json::from_value::<Message>(value)
            .unwrap()
            .replied_to()
    };
    let target = Some(Snowflake(7));
    assert_eq!(
        with(
            19,
            serde_json::json!({"message_id": "7", "channel_id": "500"})
        ),
        target
    );
    assert_eq!(
        with(19, serde_json::json!({"message_id": "7"})),
        target,
        "the channel is implied"
    );
    // A reference to another channel (a crosspost or forward source), a
    // reference on a message that is not a reply, and a reference without a
    // message are not reply targets.
    assert_eq!(
        with(
            19,
            serde_json::json!({"message_id": "7", "channel_id": "501"})
        ),
        None
    );
    assert_eq!(
        with(
            0,
            serde_json::json!({"message_id": "7", "channel_id": "500"})
        ),
        None
    );
    assert_eq!(with(19, serde_json::json!({"channel_id": "500"})), None);
    assert_eq!(with(19, serde_json::Value::Null), None);
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
