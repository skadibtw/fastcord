//! The real `fastcord-discord` client (built with `fake-discord`) against the
//! fake over loopback: REST, Gateway to a `Store`, a send answered by its own
//! `MESSAGE_CREATE`, and a 429 the rate limiter waits out. One test, because
//! the fake origin is process-wide and set once.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fastcord_discord::gateway::{BUNDLED_BUILD_NUMBER, ClientProperties, Dispatch, HostOs};
use fastcord_discord::state::Store;
use fastcord_discord::{
    Gateway, GatewayEvent, MentionPolicy, NonceGenerator, OutgoingMessage, Priority, RestClient,
    UserToken, fake_origin,
};
use fastcord_fake::{Config, start};
use fastcord_model::{Snowflake, User};
use tokio::time::timeout;

const GENERAL: Snowflake = Snowflake(6005);

fn properties() -> ClientProperties {
    ClientProperties::web(HostOs::Linux, "en-US", BUNDLED_BUILD_NUMBER)
}

async fn next(gateway: &mut Gateway) -> GatewayEvent {
    timeout(Duration::from_secs(20), gateway.next_event())
        .await
        .expect("no gateway event within 20 s")
        .expect("the gateway ended")
}

#[tokio::test]
async fn the_real_client_talks_to_the_fake() {
    let log_path =
        std::env::temp_dir().join(format!("fastcord-fake-test-{}.jsonl", std::process::id()));
    let fake = start(Config {
        port: 0,
        world: None,
        log: Some(log_path.clone()),
        echo: false,
        chatter: None,
        switches: Default::default(),
    })
    .await
    .unwrap();
    // Anything but loopback is refused, and the origin cannot change afterwards.
    assert!(fake_origin::set_origin("discord.com:443").is_err());
    fake_origin::set_origin(&format!("127.0.0.1:{}", fake.port())).unwrap();
    assert!(fake_origin::set_origin("127.0.0.1:1").is_err());

    // REST.
    let token = Arc::new(UserToken::new("fake-token".to_owned()));
    let client = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("fake-token".to_owned()),
        properties(),
    )
    .unwrap();
    let me: User = client.get("/users/@me", Priority::UserRead).await.unwrap();
    assert_eq!(me.id, Snowflake(1001));
    let bad = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("wrong".to_owned()),
        properties(),
    )
    .unwrap();
    assert!(
        bad.get::<User>("/users/@me", Priority::UserRead)
            .await
            .is_err()
    );

    // Gateway: READY decoded into the Store.
    let mut gateway = Gateway::start(client.clone(), token, properties());
    let mut store = Store::new();
    loop {
        let event = next(&mut gateway).await;
        let ready = matches!(
            &event,
            GatewayEvent::Dispatch {
                event: Dispatch::Ready(_),
                ..
            }
        );
        store.apply(event);
        if ready {
            break;
        }
    }
    assert_eq!(store.guild_ids().len(), 3, "the world has three servers");
    assert_eq!(store.current_user().map(|u| u.id), Some(Snowflake(1001)));
    assert_eq!(store.private_channels().count(), 3);
    assert_eq!(
        store.relationships().len(),
        5,
        "four friends and one request"
    );
    assert!(store.presences_held() > 0);
    assert!(
        store
            .guild(Snowflake(5001))
            .is_some_and(|g| g.name() == "Fake Admin Lab")
    );

    // A send is stored by the fake and comes back as MESSAGE_CREATE with its nonce.
    let nonce = NonceGenerator::default().next_now();
    let outgoing =
        OutgoingMessage::new("hello from the test", nonce, MentionPolicy::default()).unwrap();
    let created = client.create_message(GENERAL, &outgoing).await.unwrap();
    assert!(created.nonce.as_deref().is_some_and(|n| nonce.matches(n)));
    let echoed = loop {
        match next(&mut gateway).await {
            GatewayEvent::Dispatch {
                event: Dispatch::MessageCreate(message),
                ..
            } if message.nonce.as_deref().is_some_and(|n| nonce.matches(n)) => {
                break message;
            }
            _ => {}
        }
    };
    assert_eq!(echoed.id, created.id);
    assert_eq!(echoed.content, "hello from the test");

    // Eight quick reads of one route: the fake answers 429 after five a second,
    // and the client waits as told and still gets every answer.
    let started = Instant::now();
    for _ in 0..8 {
        let page: serde_json::Value = client
            .get(
                &format!("/channels/{GENERAL}/messages?limit=1"),
                Priority::UserRead,
            )
            .await
            .unwrap();
        assert_eq!(page.as_array().map(Vec::len), Some(1));
    }
    assert!(fake.rate_limited() >= 1, "the fake never rate limited");
    assert!(
        started.elapsed() >= Duration::from_millis(500),
        "the client did not back off"
    );

    // The request log has the traffic, with the credential redacted.
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("\"path\":\"/api/v10/users/@me\""));
    assert!(log.contains("\"kind\":\"ws\""));
    assert!(log.contains("\"RATE_LIMITED\""));
    assert!(!log.contains("fake-token"), "a token reached the log");
    drop(gateway);
    let _ = std::fs::remove_file(log_path);
}
