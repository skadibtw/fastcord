//! Real app worker timing and recovery against the loopback fake (#176).
use std::sync::Arc;
use std::time::{Duration, Instant};

use fastcord_app::testkit::{Core, View};
use fastcord_discord::gateway::{ClientProperties, HostOs};
use fastcord_discord::{RestClient, UserToken, fake_origin};
use fastcord_fake::{Config, Switches, start};
use fastcord_model::Snowflake;
use serde_json::Value;

const CHANNEL: Snowflake = Snowflake(6005);

async fn until(core: &Core, done: impl Fn(&View) -> bool) -> View {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let view = core.view();
        if done(&view) {
            return view;
        }
        assert!(
            Instant::now() < deadline,
            "core view did not converge: {view:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn entries(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
fn acks(path: &std::path::Path) -> Vec<Value> {
    entries(path)
        .into_iter()
        .filter(|e| {
            e["method"] == "POST"
                && e["path"].as_str().is_some_and(|p| {
                    p.starts_with("/api/v9/channels/6005/messages/") && p.ends_with("/ack")
                })
        })
        .collect()
}
async fn wait_log(path: &std::path::Path, predicate: impl Fn(&[Value]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if predicate(&entries(path)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "expected fake request not observed"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn core_debounces_fetches_coalesces_acks_and_survives_resume() {
    let log =
        std::env::temp_dir().join(format!("fastcord-core-timing-{}.jsonl", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let fake = start(Config {
        port: 0,
        world: None,
        log: Some(log.clone()),
        echo: false,
        chatter: None,
        switches: Switches {
            close_gateway_every: Some(Duration::from_secs(8)),
            ..Default::default()
        },
    })
    .await
    .unwrap();
    fake_origin::set_origin(&format!("127.0.0.1:{}", fake.port())).unwrap();
    let rest = RestClient::new(
        fastcord_net::NetworkContext::off(),
        UserToken::new("fake-token".into()),
        ClientProperties::bundled(HostOs::Linux, "en-US"),
    )
    .unwrap();
    let core = Core::start(rest, Arc::new(UserToken::new("fake-token".into())));
    until(&core, |v| v.ready).await;

    // Wait for each selection to reach the actual worker, then replace it
    // before its 200ms fetch delay expires. A watch-channel collapse alone
    // cannot satisfy this test: every intermediate selection was published.
    for channel in [Snowflake(6006), Snowflake(6007)] {
        core.open_channel(channel);
        until(&core, |v| v.channel == Some(channel)).await;
    }
    core.open_channel(CHANNEL);
    until(&core, |v| {
        v.channel == Some(CHANNEL) && !v.messages.is_empty()
    })
    .await;
    let reads = entries(&log);
    for channel in [6006, 6007] {
        let path = format!("/api/v9/channels/{channel}/messages");
        assert!(
            !reads
                .iter()
                .any(|e| e["method"] == "GET" && e["path"] == path),
            "intermediate channel fetched"
        );
    }
    assert_eq!(
        reads
            .iter()
            .filter(|e| e["method"] == "GET" && e["path"] == "/api/v9/channels/6005/messages")
            .count(),
        1
    );

    wait_log(&log, |rows| {
        rows.iter()
            .any(|e| e["path"].as_str().is_some_and(|p| p.ends_with("/ack")))
    })
    .await;
    let initial = acks(&log);
    assert_eq!(initial.len(), 1);
    for text in ["coalesced first", "coalesced newest"] {
        // Own sends advance the reducer cursor locally and need no ack POST.
        // Exercise the real incoming-message acknowledgement path instead.
        fake.post_friend_message("6005", text);
        until(&core, |v| {
            v.unconfirmed == 0 && v.messages.iter().any(|m| m.1 == text)
        })
        .await;
    }
    let newest = core
        .view()
        .messages
        .into_iter()
        .find(|m| m.1 == "coalesced newest")
        .unwrap()
        .0;
    let target = format!("/api/v9/channels/6005/messages/{newest}/ack");
    wait_log(&log, |rows| {
        rows.iter()
            .any(|e| e["method"] == "POST" && e["path"] == target)
    })
    .await;
    let acknowledged = acks(&log);
    assert_eq!(
        acknowledged.len(),
        2,
        "intermediate message must coalesce into newest acknowledgement"
    );
    let elapsed =
        acknowledged[1]["ts_ms"].as_u64().unwrap() - acknowledged[0]["ts_ms"].as_u64().unwrap();
    assert!(
        elapsed >= 2900,
        "automatic ack interval was only {elapsed}ms"
    );
    assert_eq!(fake.ack_token_mismatches(), 0);

    until(&core, |v| !v.ready).await;
    wait_log(&log, |rows| rows.iter().any(|e| e["method"] == "OP6")).await;
    until(&core, |v| v.ready).await;
    // A new message after resume must still travel through this same Core and
    // reconcile its outbox once; transport-only tests cannot establish this.
    assert!(core.send(CHANNEL, "after resume"));
    let view = until(&core, |v| {
        v.unconfirmed == 0 && v.messages.iter().any(|m| m.1 == "after resume")
    })
    .await;
    assert_eq!(view.channel, Some(CHANNEL));
    assert_eq!(
        view.messages
            .iter()
            .filter(|m| m.1 == "after resume")
            .count(),
        1
    );
    assert!(view.messages.iter().any(|m| m.0 == newest));
    assert_eq!(
        entries(&log)
            .iter()
            .filter(|e| e["method"] == "OP2")
            .count(),
        1
    );
    drop(core);
    drop(fake);
    let _ = std::fs::remove_file(log);
}
