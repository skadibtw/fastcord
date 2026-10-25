//! The real SOCKS5 association under the production media UDP adapter. All
//! network sockets are loopback; the public media destination is encapsulated
//! only and is never contacted by this fixture.
use super::*;
use fastcord_net::{NetworkContext, ProxyConfig, ProxyKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use zeroize::Zeroizing;

#[tokio::test]
async fn discovery_uses_the_relay_external_address_and_control_loss_ends_the_media_join() {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let relay = UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let bound = relay.local_addr().unwrap();
    let context = NetworkContext::off()
        .with_proxy(
            ProxyConfig {
                kind: ProxyKind::Socks5,
                host: "127.0.0.1".into(),
                port: listener.local_addr().unwrap().port(),
                username: None,
            },
            Zeroizing::new(String::new()),
        )
        .unwrap();
    let fixture = tokio::spawn(async move {
        let (mut control, _) = listener.accept().await.unwrap();
        let mut hello = [0; 3];
        control.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello, [5, 1, 0]);
        control.write_all(&[5, 0]).await.unwrap();
        let mut request = [0; 10];
        control.read_exact(&mut request).await.unwrap();
        assert_eq!(&request[..8], &[5, 3, 0, 1, 0, 0, 0, 0]);
        assert_eq!(u16::from_be_bytes([request[8], request[9]]), 0);
        let mut reply = vec![5, 0, 0, 1, 127, 0, 0, 1];
        reply.extend_from_slice(&bound.port().to_be_bytes());
        control.write_all(&reply).await.unwrap();
        let mut packet = [0; 1500];
        let (length, from) = relay.recv_from(&mut packet).await.unwrap();
        assert_eq!(&packet[..8], &[0, 0, 0, 1, 1, 1, 1, 1]);
        assert_eq!(u16::from_be_bytes([packet[8], packet[9]]), 50001);
        assert_eq!(length, 10 + DISCOVERY_LEN);
        assert_ne!(from.port(), 0);
        assert_eq!(&packet[10..18], &[0, 1, 0, 70, 0, 0, 0x32, 0x47]);
        let mut response = packet[..length].to_vec();
        response[11] = 2;
        let external = b"203.0.113.222";
        response[18..18 + external.len()].copy_from_slice(external);
        response[length - 2..].copy_from_slice(&41001u16.to_be_bytes());
        relay.send_to(&response, from).await.unwrap();
        control
    });
    let signaling = FakeSignaling::default();
    let mut ws = signaling.accept().await;
    let session = start(
        signaling.clone(),
        crate::gateway::transport::LiveNetwork(context.clone()),
        credentials("fixture.discord.media:443"),
        options(),
    );
    ws.hello(QUIET_HEARTBEAT_MS).await;
    assert_eq!(ws.recv_op(0).await["op"], 0);
    let mut ready = ready(&BOTH_MODES);
    ready["d"]["ip"] = json!("1.1.1.1");
    ws.send(ready).await;
    let select = timeout(Duration::from_secs(3), ws.recv_op(1))
        .await
        .unwrap();
    assert_eq!(select["d"]["data"]["address"], "203.0.113.222");
    assert_eq!(select["d"]["data"]["port"], 41001);
    assert_eq!(session.status(), VoiceStatus::Negotiating);
    let mut status = session.watch_status();
    drop(fixture.await.unwrap());
    let closed = timeout(Duration::from_secs(1), async {
        loop {
            if let VoiceStatus::Closed(reason) = *status.borrow_and_update() {
                break reason;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(closed, CloseReason::UdpRelayClosed);
    assert_eq!(
        signaling.urls().len(),
        1,
        "dead relay must not start a new voice Identify/Resume"
    );
    assert!(!context.experimental_udp());
    session.leave().await;
}
