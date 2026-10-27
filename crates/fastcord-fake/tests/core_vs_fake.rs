//! The UI-independent client core of `fastcord-app` (reducer worker, history
//! coordinator, outbox; through `fastcord_app::testkit`) against the fake over
//! loopback: READY, open a channel, a send confirmed by its own MESSAGE_CREATE
//! (nonce matched, outbox emptied), an edit and a delete. One test, because the
//! fake origin is process-wide and set once (#176).

use std::time::{Duration, Instant};

use fastcord_app::testkit::{Core, View};
use fastcord_discord::gateway::{ClientProperties, HostOs};
use fastcord_discord::{RestClient, UserToken, fake_origin};
use fastcord_fake::{Config, start};
use fastcord_model::Snowflake;
use std::sync::Arc;

const GENERAL: Snowflake = Snowflake(6005);

/// Polls the published view until `done`, within a bound.
async fn until(core: &Core, what: &str, done: impl Fn(&View) -> bool) -> View {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = core.view();
        if done(&view) {
            return view;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn the_client_core_talks_to_the_fake() {
    let fake = start(Config {
        port: 0,
        world: None,
        log: None,
        echo: false,
        chatter: None,
        switches: Default::default(),
    })
    .await
    .unwrap();
    fake_origin::set_origin(&format!("127.0.0.1:{}", fake.port())).unwrap();
    let rest = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("fake-token".to_owned()),
        ClientProperties::bundled(HostOs::Linux, "en-US"),
    )
    .unwrap();
    let core = Core::start(rest, Arc::new(UserToken::new("fake-token".to_owned())));

    until(&core, "READY", |v| v.ready).await;
    core.open_channel(GENERAL);
    until(&core, "the channel's history", |v| {
        v.channel == Some(GENERAL) && !v.messages.is_empty()
    })
    .await;

    // Send: confirmed by the echo of its own nonce, so the outbox empties.
    assert!(core.send(GENERAL, "hello from the core test"));
    let view = until(&core, "the sent message", |v| {
        v.unconfirmed == 0 && v.messages.iter().any(|m| m.1 == "hello from the core test")
    })
    .await;
    assert!(!view.send_trouble);
    let id = view
        .messages
        .iter()
        .find(|m| m.1 == "hello from the core test")
        .unwrap()
        .0;
    assert_eq!(
        view.messages
            .iter()
            .filter(|m| m.1 == "hello from the core test")
            .count(),
        1,
        "the confirmed send must replace its outbox row, not duplicate"
    );

    // Edit, then delete.
    assert!(core.edit(GENERAL, id, "edited by the core test"));
    until(&core, "the edit", |v| {
        v.messages
            .iter()
            .any(|m| m.0 == id && m.1 == "edited by the core test")
    })
    .await;
    assert!(core.delete(GENERAL, id));
    until(&core, "the deletion", |v| {
        v.messages.iter().all(|m| m.0 != id)
    })
    .await;
}
