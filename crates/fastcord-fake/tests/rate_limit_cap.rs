//! A server that keeps answering 429 with `retry_after: 0`: the real client
//! waits at least a second between tries and gives up after five. One test per
//! file, because the fake origin is process-wide and set once.

use std::time::{Duration, Instant};

use fastcord_discord::gateway::{BUNDLED_BUILD_NUMBER, ClientProperties, HostOs};
use fastcord_discord::{Priority, RestClient, RestError, UserToken, fake_origin};
use fastcord_fake::{Config, Switches, start};

#[tokio::test]
async fn five_zero_delay_429s_become_a_rate_limited_error_after_waiting_between_them() {
    let fake = start(Config {
        port: 0,
        world: None,
        log: None,
        echo: false,
        chatter: None,
        switches: Switches {
            zero_retry_429: true,
            ..Switches::default()
        },
    })
    .await
    .unwrap();
    fake_origin::set_origin(&format!("127.0.0.1:{}", fake.port())).unwrap();
    let client = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("fake-token".to_owned()),
        ClientProperties::web(HostOs::Linux, "en-US", BUNDLED_BUILD_NUMBER),
    )
    .unwrap();

    let started = Instant::now();
    let answer = client
        .get::<serde_json::Value>("/users/@me", Priority::UserRead)
        .await;
    let waited = started.elapsed();
    assert_eq!(answer, Err(RestError::RateLimited));
    assert_eq!(fake.rate_limited(), 5, "five requests, not an endless loop");
    // Four waits of the 1 s minimum between five tries.
    assert!(waited >= Duration::from_millis(3_900), "{waited:?}");
    assert!(waited < Duration::from_secs(15), "{waited:?}");
}
