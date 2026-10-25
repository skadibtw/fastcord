//! `GET /gateway` answering 403 twice in a row stops the real Gateway with a
//! manual-reconnect state instead of probing forever. One test per file: the
//! fake origin is process-wide.

use std::sync::Arc;
use std::time::Duration;

use fastcord_discord::gateway::{ClientProperties, ConnectionState, HostOs, StopReason};
use fastcord_discord::{Gateway, GatewayEvent, RestClient, UserToken, fake_origin};
use fastcord_fake::{Config, Switches, start};
use tokio::time::timeout;

#[tokio::test]
async fn two_gateway_discovery_403s_stop_with_the_manual_reconnect_state() {
    let log_path = std::env::temp_dir().join(format!(
        "fastcord-fake-403-test-{}.jsonl",
        std::process::id()
    ));
    let fake = start(Config {
        port: 0,
        world: None,
        log: Some(log_path.clone()),
        echo: false,
        chatter: None,
        switches: Switches {
            forbid_gateway_discovery: true,
            ..Switches::default()
        },
    })
    .await
    .unwrap();
    fake_origin::set_origin(&format!("127.0.0.1:{}", fake.port())).unwrap();
    let properties = ClientProperties::bundled(HostOs::Linux, "en-US");
    let client = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("fake-token".to_owned()),
        properties.clone(),
    )
    .unwrap();
    let mut gateway = Gateway::start(client, Arc::new(UserToken::new("fake-token".to_owned())));
    let mut last = None;
    while let Some(event) = timeout(Duration::from_secs(20), gateway.next_event())
        .await
        .expect("the gateway kept retrying")
    {
        last = Some(event);
    }
    assert_eq!(
        last,
        Some(GatewayEvent::State(ConnectionState::Stopped(
            StopReason::DiscoveryForbidden
        )))
    );
    assert!(StopReason::DiscoveryForbidden.allows_manual_reconnect());
    let log = std::fs::read_to_string(&log_path).unwrap();
    let discoveries = log
        .lines()
        .filter(|line| line.contains("\"path\":\"/api/v9/gateway\""))
        .count();
    assert_eq!(discoveries, 2, "exactly two asks, then it stopped");
    assert!(!log.contains("\"kind\":\"ws\""), "no socket was opened");
    let _ = std::fs::remove_file(log_path);
}
