use super::*;
use crate::gateway::{ClientProperties, HostOs};
use crate::{Priority, RestClient, RestError, RestRequest, Route, UserToken};
use WriteGroup::*;

fn classify(method: Method, path: &str) -> WriteGroup {
    let route = Route::new(method.clone(), path).unwrap();
    WriteGroup::classify(&method, route.key().normalized(), None)
}

fn client() -> RestClient {
    RestClient::new(
        UserToken::new("offline-test-credential".to_owned()),
        ClientProperties::bundled(HostOs::Linux, "en-US"),
    )
    .unwrap()
}

#[test]
fn every_write_route_the_app_builds_has_its_group() {
    use Method as M;
    let table: &[(Method, &str, WriteGroup)] = &[
        (M::POST, "/attachments/refresh-urls", AttachmentRefresh),
        (M::PATCH, "/channels/1/messages/2", BasicMessages),
        (M::DELETE, "/channels/1/messages/2", BasicMessages),
        (M::POST, "/channels/1/messages/2/ack", Ack),
        (M::POST, "/channels/1/typing", Typing),
        (
            M::PUT,
            "/channels/1/messages/2/reactions/%F0%9F%91%8D/@me",
            OwnReactions,
        ),
        (
            M::DELETE,
            "/channels/1/messages/2/reactions/%F0%9F%91%8D/0/@me",
            OwnReactions,
        ),
        (
            M::DELETE,
            "/channels/1/messages/2/reactions/%F0%9F%91%8D/77",
            Moderation,
        ),
        (
            M::DELETE,
            "/channels/1/messages/2/reactions/%F0%9F%91%8D",
            Moderation,
        ),
        (M::POST, "/channels/1/messages/bulk-delete", Moderation),
        (M::POST, "/channels/1/attachments", Uploads),
        (M::PUT, "/channels/1/pins/2", Pins),
        (M::DELETE, "/channels/1/pins/2", Pins),
        (M::PUT, "/channels/1/polls/2/answers/@me", PollVotes),
        (M::POST, "/channels/1/polls/2/expire", PollVotes),
        (M::POST, "/interactions", Interactions),
        (M::POST, "/channels/1/call/ring", CallRing),
        (M::POST, "/channels/1/call/stop-ringing", CallRing),
        (M::POST, "/channels/1/send-soundboard-sound", SoundboardPlay),
        (M::POST, "/users/@me/channels", OpenDm),
        (M::PUT, "/channels/1/recipients/2", GroupDm),
        (M::DELETE, "/channels/1/recipients/2", GroupDm),
        (M::PUT, "/channels/1/recipients/@me", Relationships),
        (M::PUT, "/users/@me/relationships/2", Relationships),
        (M::DELETE, "/users/@me/relationships/2", Relationships),
        (M::PUT, "/users/@me/relationships/2/ignore", Relationships),
        (
            M::DELETE,
            "/users/@me/relationships/2/ignore",
            Relationships,
        ),
        (M::POST, "/users/@me/relationships", FriendRequest),
        (M::POST, "/invites/abc123", InviteJoin),
        (M::DELETE, "/users/@me/guilds/5", InviteJoin),
        (M::POST, "/channels/1/invites", InviteManage),
        (M::DELETE, "/invites/abc123", InviteManage),
        (M::PATCH, "/users/@me", Profile),
        (M::PATCH, "/users/@me/profile", Profile),
        (M::PATCH, "/users/@me/connections/github/123", Profile),
        (M::PATCH, "/guilds/5/members/@me", Profile),
        (M::PATCH, "/users/@me/settings", UserSettings),
        (M::PATCH, "/users/@me/guilds/5/settings", UserSettings),
        (M::PATCH, "/users/@me/guilds/@me/settings", UserSettings),
        (M::POST, "/auth/sessions/logout", SessionsLogout),
        (M::DELETE, "/oauth2/tokens/9", OAuthDeauthorize),
        (M::POST, "/reporting/message", Reports),
        (M::POST, "/guilds", ServerAdmin),
        (M::PATCH, "/guilds/5", ServerAdmin),
        (M::DELETE, "/guilds/5", ServerAdmin),
        (M::POST, "/guilds/5/mfa", ServerAdmin),
        (M::POST, "/guilds/5/channels", Channels),
        (M::PATCH, "/guilds/5/channels", Channels),
        (M::PATCH, "/channels/1", Channels),
        (M::DELETE, "/channels/1", Channels),
        (M::POST, "/guilds/5/roles", Roles),
        (M::PATCH, "/guilds/5/roles", Roles),
        (M::PATCH, "/guilds/5/roles/6", Roles),
        (M::DELETE, "/guilds/5/roles/6", Roles),
        (M::PUT, "/guilds/5/members/2/roles/6", Roles),
        (M::DELETE, "/guilds/5/members/2/roles/6", Roles),
        (M::PUT, "/channels/1/permissions/2", Roles),
        (M::DELETE, "/channels/1/permissions/2", Roles),
        (M::PATCH, "/guilds/5/members/2", Moderation),
        (M::DELETE, "/guilds/5/members/2", Moderation),
        (M::PUT, "/guilds/5/bans/2", Moderation),
        (M::DELETE, "/guilds/5/bans/2", Moderation),
        (M::POST, "/guilds/5/auto-moderation/rules", AutoMod),
        (M::PATCH, "/guilds/5/auto-moderation/rules/7", AutoMod),
        (M::DELETE, "/guilds/5/auto-moderation/rules/7", AutoMod),
        (M::PATCH, "/guilds/5/member-verification", CommunityConfig),
        (M::PUT, "/guilds/5/onboarding", CommunityConfig),
        (M::PUT, "/guilds/5/new-member-welcome", CommunityConfig),
        (M::PUT, "/guilds/5/requests/@me", Onboarding),
        (M::PUT, "/guilds/5/onboarding-responses", Onboarding),
        (M::POST, "/guilds/5/scheduled-events", ScheduledEvents),
        (M::PATCH, "/guilds/5/scheduled-events/8", ScheduledEvents),
        (M::DELETE, "/guilds/5/scheduled-events/8", ScheduledEvents),
        (
            M::PUT,
            "/guilds/5/scheduled-events/8/users/@me",
            ScheduledEvents,
        ),
        (
            M::DELETE,
            "/guilds/5/scheduled-events/8/users/@me",
            ScheduledEvents,
        ),
        (M::POST, "/channels/1/messages/2/crosspost", Announcements),
        (M::POST, "/channels/1/followers", Announcements),
        (M::POST, "/guilds/5/emojis", Expressions),
        (M::PATCH, "/guilds/5/emojis/6", Expressions),
        (M::DELETE, "/guilds/5/stickers/6", Expressions),
        (M::POST, "/guilds/5/stickers", Expressions),
        (M::POST, "/guilds/5/soundboard-sounds", Expressions),
        (M::DELETE, "/guilds/5/soundboard-sounds/7", Expressions),
        (M::POST, "/channels/1/webhooks", Webhooks),
        (M::PATCH, "/webhooks/9", Webhooks),
        (M::DELETE, "/webhooks/9", Webhooks),
        (M::DELETE, "/guilds/5/integrations/9", Webhooks),
        (M::PATCH, "/guilds/5/voice-states/@me", Stage),
        (M::PATCH, "/guilds/5/voice-states/2", Stage),
        (M::POST, "/stage-instances", Stage),
        (M::PATCH, "/stage-instances/1", Stage),
        (M::DELETE, "/stage-instances/1", Stage),
        (M::POST, "/channels/1/threads", Threads),
        (M::POST, "/channels/1/messages/2/threads", Threads),
        (M::PUT, "/channels/1/thread-members/@me", Threads),
        (M::DELETE, "/channels/1/thread-members/@me", Threads),
    ];
    for (method, path, group) in table {
        assert_eq!(classify(method.clone(), path), *group, "{method} {path}");
    }
}

#[test]
fn shared_routes_take_the_group_their_caller_names() {
    let request = |group: Option<WriteGroup>| {
        let r = RestRequest::new(
            Route::new(Method::PATCH, "/channels/1").unwrap(),
            Priority::UserWrite,
        );
        match group {
            Some(g) => r.in_group(g),
            None => r,
        }
        .write_group()
    };
    assert_eq!(request(None), Channels);
    assert_eq!(request(Some(GroupDm)), GroupDm);
    assert_eq!(request(Some(Threads)), Threads);
}

#[test]
fn exactly_the_verified_groups_are_on_by_default() {
    let on: Vec<_> = WriteGroup::ALL
        .iter()
        .copied()
        .filter(|g| g.default_on())
        .collect();
    assert_eq!(
        on,
        [BasicMessages, OwnReactions, Typing, Ack, AttachmentRefresh]
    );
    assert!(WriteGroup::ALL.len() <= 64);
    let client = client();
    for group in WriteGroup::ALL {
        assert_eq!(
            client.write_group_enabled(*group),
            group.default_on(),
            "{group:?}"
        );
    }
}

#[test]
fn unknown_writes_are_unclassified_and_off() {
    for (method, path) in [
        (Method::POST, "/something/new"),
        (Method::PUT, "/channels/1/unheard-of"),
        (Method::DELETE, "/users/@me/channels"),
        (Method::PATCH, "/channels/1/messages"),
        (Method::POST, "/channels/1/messages/2/ack/extra"),
        (Method::OPTIONS, "/channels/1/typing"),
    ] {
        let group = classify(method.clone(), path);
        assert_eq!(group, Unclassified, "{method} {path}");
        assert!(!group.default_on());
        assert!(!client().write_group_enabled(group));
    }
    // A message without a readable body is not a basic message either.
    let messages = "/channels/{channel_id}/messages";
    assert_eq!(
        WriteGroup::classify(&Method::POST, messages, None),
        Unclassified
    );
    assert_eq!(
        WriteGroup::classify(&Method::POST, messages, Some(b"nope")),
        Unclassified
    );
}

#[test]
fn message_bodies_pick_their_group() {
    use serde_json::json;
    let group = |body: serde_json::Value| {
        WriteGroup::classify(
            &Method::POST,
            "/channels/{channel_id}/messages",
            Some(body.to_string().as_bytes()),
        )
    };
    assert_eq!(
        group(json!({"content": "hi", "nonce": "1", "tts": false})),
        BasicMessages
    );
    assert_eq!(
        group(json!({"content": "hi", "message_reference": {"type": 0, "message_id": "2"}})),
        BasicMessages
    );
    assert_eq!(
        group(json!({"content": "hi", "flags": 4096})),
        BasicMessages
    );
    assert_eq!(group(json!({"content": "x", "tts": true})), MessageExtras);
    assert_eq!(
        group(json!({"poll": {"question": {"text": "q"}, "answers": [], "duration": 24}})),
        MessageExtras
    );
    assert_eq!(group(json!({"sticker_ids": ["77"]})), MessageExtras);
    assert_eq!(
        group(json!({"sticker_ids": [], "content": "hi"})),
        BasicMessages
    );
    assert_eq!(
        group(json!({"message_reference": {"type": 1, "message_id": "2", "channel_id": "3"}})),
        MessageExtras
    );
    assert_eq!(
        group(json!({"content": "", "attachments": [{"id": 0}]})),
        Uploads
    );
    assert_eq!(group(json!({"attachments": []})), BasicMessages);
}

#[tokio::test]
async fn a_blocked_write_fails_before_the_limiter_and_enabling_lets_it_through() {
    use std::task::{Context, Poll, Waker};

    let client = client();
    let route = Route::new(Method::PUT, "/channels/1/pins/2").unwrap();
    // The route is busy: a request that reached the limiter would wait here.
    let busy = client
        .limiter()
        .acquire(route.key().clone(), Priority::UserWrite, 1024)
        .await
        .unwrap();
    let pin = || RestRequest::new(route.clone(), Priority::UserWrite);
    let mut context = Context::from_waker(Waker::noop());

    let mut blocked = Box::pin(client.execute(pin()));
    assert!(matches!(
        blocked.as_mut().poll(&mut context),
        Poll::Ready(Err(RestError::WriteNotVerified(Pins)))
    ));
    assert_eq!(
        RestError::WriteNotVerified(Pins).to_string(),
        "Not yet verified on a live account. Enable it under Settings \u{2192} Advanced \u{2192} Test unverified actions (alt account only)."
    );

    client.set_write_group(Pins, true);
    let mut allowed = Box::pin(client.execute(pin()));
    assert!(
        allowed.as_mut().poll(&mut context).is_pending(),
        "queued at the limiter"
    );
    // Clones share the switch, and it can be turned off again.
    let clone = client.clone();
    clone.set_write_group(Pins, false);
    assert!(!client.write_group_enabled(Pins));
    drop(allowed);
    drop(busy);
}
