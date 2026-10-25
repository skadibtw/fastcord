//! A Gateway that closes every second: the real client resumes (one Identify
//! only), and never reconnects without a wait. One test per file: the fake
//! origin is process-wide.

use std::sync::Arc;
use std::time::Duration;

use fastcord_discord::gateway::{ClientProperties, ConnectionState, HostOs, ReconnectReason};
use fastcord_discord::{Gateway, GatewayEvent, RestClient, UserToken, fake_origin};
use fastcord_fake::{Config, Switches, start};
use serde_json::Value;
use tokio::time::timeout;

#[tokio::test]
async fn closes_every_second_are_resumed_with_one_identify_and_a_wait_each_time() {
    let log_path = std::env::temp_dir().join(format!(
        "fastcord-fake-resume-test-{}.jsonl",
        std::process::id()
    ));
    let fake = start(Config {
        port: 0,
        world: None,
        log: Some(log_path.clone()),
        echo: false,
        chatter: None,
        switches: Switches {
            close_gateway_every: Some(Duration::from_secs(1)),
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
    let mut delays = Vec::new();
    while delays.len() < 2 {
        let event = timeout(Duration::from_secs(20), gateway.next_event())
            .await
            .expect("no event")
            .expect("the gateway ended");
        if let GatewayEvent::State(ConnectionState::Reconnecting { delay, reason, .. }) = event {
            assert_eq!(reason, ReconnectReason::Closed(Some(4000)));
            delays.push(delay);
        }
    }
    // Let the second resume reach the log.
    tokio::time::sleep(delays[1] + Duration::from_millis(700)).await;
    assert!(
        delays
            .iter()
            .all(|delay| *delay >= Duration::from_millis(500)),
        "{delays:?}"
    );

    let log = std::fs::read_to_string(&log_path).unwrap();
    let entries: Vec<Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .filter(|entry: &Value| entry["kind"] == "ws")
        .collect();
    let count = |method: &str| entries.iter().filter(|e| e["method"] == method).count();
    assert_eq!(count("OP2"), 1, "one Identify; every later connect resumes");
    assert!(count("OP6") >= 2, "resumes: {}", count("OP6"));
    // Each CLOSE is followed by the next CONNECT only after a real wait.
    let mut gaps = 0;
    for (index, entry) in entries.iter().enumerate() {
        if entry["method"] != "CLOSE" {
            continue;
        }
        let Some(next) = entries[index + 1..]
            .iter()
            .find(|e| e["method"] == "CONNECT")
        else {
            continue;
        };
        let gap = next["ts_ms"].as_u64().unwrap() - entry["ts_ms"].as_u64().unwrap();
        assert!(gap >= 400, "reconnected {gap} ms after a close");
        gaps += 1;
    }
    assert!(gaps >= 2, "{gaps}");
    drop(gateway);
    let _ = std::fs::remove_file(log_path);
}
