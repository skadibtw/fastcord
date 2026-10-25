//! The ack token Discord's answer carries is kept and sent with the next
//! automatic ack. One test per file: the fake origin is process-wide.

use fastcord_discord::gateway::{ClientProperties, HostOs};
use fastcord_discord::{RestClient, UserToken, fake_origin};
use fastcord_fake::{Config, start};
use fastcord_model::Snowflake;

const GENERAL: Snowflake = Snowflake(6005);

#[tokio::test]
async fn the_next_ack_sends_back_the_token_of_the_last_answer() {
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
    let client = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("fake-token".to_owned()),
        ClientProperties::bundled(HostOs::Linux, "en-US"),
    )
    .unwrap();

    let first = client
        .ack_message(GENERAL, Snowflake(7000), false, None)
        .await
        .unwrap();
    assert_eq!(first.as_deref(), Some("fake-ack-1"));
    let second = client
        .ack_message(GENERAL, Snowflake(7001), false, first.as_deref())
        .await
        .unwrap();
    assert_eq!(second.as_deref(), Some("fake-ack-2"));
    assert_eq!(fake.ack_token_mismatches(), 0, "the token was echoed");
    // A manual "mark as read" is immediate, carries no token, and still
    // yields the next one.
    let manual = client
        .ack_message(GENERAL, Snowflake(7002), true, None)
        .await
        .unwrap();
    assert_eq!(manual.as_deref(), Some("fake-ack-3"));
    assert_eq!(fake.ack_token_mismatches(), 0);
    // Without the token the fake notices, so the check above means something.
    client
        .ack_message(GENERAL, Snowflake(7003), false, None)
        .await
        .unwrap();
    assert_eq!(fake.ack_token_mismatches(), 1);
}
