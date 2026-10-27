//! Real loopback SOCKS control/UDP fixtures. Media destinations are only
//! encapsulated in datagrams to the local relay; no packet reaches them.
use super::*;
use crate::{NetworkContext, ProxyConfig};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use zeroize::Zeroizing;

#[derive(Default)]
struct PacketAudit {
    calls: std::sync::Mutex<Vec<(SocketAddr, u8, [u8; 74])>>,
    unavailable: std::sync::atomic::AtomicBool,
}
impl crate::tls::FakePrefixFactory for PacketAudit {
    fn available(&self) -> bool {
        !self.unavailable.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn supports_udp_discovery(&self) -> bool {
        true
    }
    fn start(
        &self,
        _: std::net::TcpStream,
        _: Vec<u8>,
        _: Vec<u8>,
        _: crate::tls::Strategy,
    ) -> io::Result<crate::tls::PrefixFuture> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn start_udp_discovery(
        &self,
        socket: std::net::UdpSocket,
        server: SocketAddr,
        request: [u8; 74],
        variant: u8,
    ) -> io::Result<crate::tls::PrefixFuture> {
        self.calls.lock().unwrap().push((server, variant, request));
        Ok(Box::pin(async move { socket.send_to(&request, server) }))
    }
}
fn discovery_request() -> [u8; 74] {
    let mut request = [0; 74];
    request[..8].copy_from_slice(&[0, 1, 0, 70, 0x12, 0x34, 0x56, 0x78]);
    request
}

#[tokio::test]
async fn only_canonical_discovery_reaches_the_packet_factory() {
    let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let server = receiver.local_addr().unwrap();
    let factory = Arc::new(PacketAudit::default());
    let socket = DatagramSocket::Packet {
        socket: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        server,
        factory: factory.clone(),
    };
    let request = discovery_request();
    assert_eq!(
        socket.send_discovery(&request, server, 2).await.unwrap(),
        74
    );
    let mut buffer = [0; 128];
    let n = timeout(Duration::from_secs(1), receiver.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(&buffer[..n], &request);
    let rtp = [0x80, 0x78, 0, 1, 0, 0, 0, 1];
    socket.send_to(&rtp, server).await.unwrap();
    let n = timeout(Duration::from_secs(1), receiver.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap()
        .0;
    assert_eq!(&buffer[..n], &rtp);
    for n in 0..74 {
        assert!(
            socket
                .send_discovery(&request[..n], server, 1)
                .await
                .is_err()
        );
    }
    for variant in [0, 4, 255] {
        assert!(
            socket
                .send_discovery(&request, server, variant)
                .await
                .is_err()
        );
    }
    let mut modified = request;
    modified[8] = 1;
    assert!(socket.send_discovery(&modified, server, 1).await.is_err());
    assert!(
        socket
            .send_discovery(&request, "127.0.0.1:1".parse().unwrap(), 1)
            .await
            .is_err()
    );
    assert_eq!(
        factory.calls.lock().unwrap().as_slice(),
        &[(server, 2, request)]
    );
    factory
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        socket
            .send_discovery(&request, server, 1)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionAborted
    );
    assert_eq!(factory.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn packet_udp_binding_is_explicit_auto_only_and_never_implicitly_elevates() {
    use crate::settings::Mode;
    let factory = Arc::new(PacketAudit::default());
    let context = NetworkContext::new(Mode::Auto).with_prefix_factory(factory.clone());
    assert!(matches!(
        context.bind_udp(media()).await.unwrap(),
        DatagramSocket::Packet { .. }
    ));
    assert!(matches!(
        context
            .with_mode(Mode::Off)
            .bind_udp(media())
            .await
            .unwrap(),
        DatagramSocket::Direct(_)
    ));
    assert!(matches!(
        NetworkContext::new(Mode::Auto)
            .bind_udp(media())
            .await
            .unwrap(),
        DatagramSocket::Direct(_)
    ));
    factory
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(
        context.bind_udp(media()).await.unwrap(),
        DatagramSocket::Direct(_)
    ));
    assert!(factory.calls.lock().unwrap().is_empty());
}

fn media() -> SocketAddr {
    "1.1.1.1:50001".parse().unwrap()
}
fn config(address: SocketAddr, auth: bool) -> ProxyConfig {
    ProxyConfig {
        kind: ProxyKind::Socks5,
        host: address.ip().to_string(),
        port: address.port(),
        username: auth.then(|| "fixture-user".into()),
    }
}

async fn associate_request(control: &mut TcpStream, auth: bool) -> SocketAddr {
    let mut hello = [0; 3];
    control.read_exact(&mut hello).await.unwrap();
    assert_eq!(hello, [5, 1, if auth { 2 } else { 0 }]);
    control.write_all(&[5, hello[2]]).await.unwrap();
    if auth {
        assert_eq!(control.read_u8().await.unwrap(), 1);
        let n = control.read_u8().await.unwrap();
        let mut name = vec![0; n as usize];
        control.read_exact(&mut name).await.unwrap();
        let n = control.read_u8().await.unwrap();
        let mut password = vec![0; n as usize];
        control.read_exact(&mut password).await.unwrap();
        assert_eq!(name, b"fixture-user");
        assert_eq!(password, b"fixture-password");
        control.write_all(&[1, 0]).await.unwrap();
    }
    let mut request = [0; 4];
    control.read_exact(&mut request).await.unwrap();
    assert_eq!(&request[..3], &[5, 3, 0]);
    let ip = match request[3] {
        1 => {
            let mut ip = [0; 4];
            control.read_exact(&mut ip).await.unwrap();
            IpAddr::V4(Ipv4Addr::from(ip))
        }
        4 => {
            let mut ip = [0; 16];
            control.read_exact(&mut ip).await.unwrap();
            IpAddr::V6(Ipv6Addr::from(ip))
        }
        _ => panic!("association must use a numeric wildcard source hint"),
    };
    let port = control.read_u16().await.unwrap();
    assert_eq!(port, 0, "proxy-visible UDP port is unknown behind NAT");
    assert!(
        ip.is_unspecified(),
        "a local IP is not proof of the NAT source"
    );
    SocketAddr::new(ip, port)
}

async fn reply(control: &mut TcpStream, bound: SocketAddr) {
    let mut response = vec![5, 0, 0];
    match bound.ip() {
        IpAddr::V4(ip) => {
            response.push(1);
            response.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            response.push(4);
            response.extend_from_slice(&ip.octets());
        }
    }
    response.extend_from_slice(&bound.port().to_be_bytes());
    // Arbitrary TCP read boundaries must not affect association parsing.
    for byte in response {
        control.write_all(&[byte]).await.unwrap();
    }
}

async fn ready(
    server: SocketAddr,
    auth: bool,
    wildcard: bool,
) -> (NetworkContext, DatagramSocket, UdpSocket, TcpStream) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = relay.local_addr().unwrap();
    let context = NetworkContext::off()
        .with_proxy(
            config(listener.local_addr().unwrap(), auth),
            Zeroizing::new(if auth {
                "fixture-password".into()
            } else {
                String::new()
            }),
        )
        .unwrap();
    let fixture = tokio::spawn(async move {
        let (mut control, _) = listener.accept().await.unwrap();
        let announced = associate_request(&mut control, auth).await;
        reply(
            &mut control,
            if wildcard {
                SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), address.port())
            } else {
                address
            },
        )
        .await;
        (control, announced)
    });
    let socket = timeout(Duration::from_secs(3), context.bind_udp(server))
        .await
        .unwrap()
        .unwrap();
    let (control, _) = fixture.await.unwrap();
    assert!(socket.is_relay());
    assert_ne!(socket.local_addr().unwrap().port(), 0);
    (context, socket, relay, control)
}

fn frame(server: SocketAddr, body: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0; header_size(server) + body.len()];
    let n = encode_datagram(server, body, &mut bytes).unwrap();
    assert_eq!(n, bytes.len());
    bytes
}

#[tokio::test]
async fn authenticated_association_binds_a_real_port_and_relays_ipv4_and_ipv6_origins() {
    for server in [
        media(),
        SocketAddr::new("2606:4700:4700::1111".parse().unwrap(), 50001),
    ] {
        let (_context, socket, relay, _control) = ready(server, true, true).await;
        assert_eq!(socket.send_to(b"discovery", server).await.unwrap(), 9);
        let mut encoded = [0; 2048];
        let (n, from) = relay.recv_from(&mut encoded).await.unwrap();
        assert_eq!(
            parse_datagram(&encoded[..n], server),
            Some(b"discovery".as_slice())
        );
        relay
            .send_to(&frame(server, b"external-address-response"), from)
            .await
            .unwrap();
        let mut body = [0; 128];
        let (n, source) = socket.recv_from(&mut body).await.unwrap();
        assert_eq!(&body[..n], b"external-address-response");
        assert_eq!(source, server);
        assert_eq!(
            socket.max_payload(),
            OUTER_IPV4_PAYLOAD - header_size(server)
        );
    }
}

#[test]
fn datagram_parser_rejects_fragments_domains_bad_reserved_bytes_and_every_short_header() {
    for server in [
        media(),
        SocketAddr::new("2606:4700:4700::1111".parse().unwrap(), 50001),
    ] {
        let valid = frame(server, b"payload");
        assert_eq!(parse_datagram(&valid, server), Some(b"payload".as_slice()));
        for end in 0..header_size(server) {
            assert!(parse_datagram(&valid[..end], server).is_none());
        }
        for field in 0..3 {
            let mut wrong = valid.clone();
            wrong[field] = 1;
            assert!(parse_datagram(&wrong, server).is_none());
        }
        for kind in [0, 2, 3, 5, 255] {
            let mut wrong = valid.clone();
            wrong[3] = kind;
            assert!(parse_datagram(&wrong, server).is_none());
        }
        assert!(parse_datagram(&valid, SocketAddr::new(server.ip(), server.port() + 1)).is_none());
        let mut too_small = vec![0; valid.len() - 1];
        assert!(encode_datagram(server, b"payload", &mut too_small).is_err());
    }
}

#[tokio::test]
async fn wrong_relay_source_wrong_inner_endpoint_malformed_and_oversized_packets_are_ignored() {
    let (_context, socket, relay, _control) = ready(media(), false, false).await;
    let destination = socket.local_addr().unwrap();
    let rogue = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    rogue
        .send_to(&frame(media(), b"wrong-relay"), destination)
        .await
        .unwrap();
    relay
        .send_to(
            &frame("8.8.8.8:50001".parse().unwrap(), b"wrong-origin"),
            destination,
        )
        .await
        .unwrap();
    for field in 0..4 {
        let mut wrong = frame(media(), b"malformed");
        wrong[field] = 255;
        relay.send_to(&wrong, destination).await.unwrap();
    }
    relay
        .send_to(&vec![0; OUTER_IPV4_PAYLOAD + 20], destination)
        .await
        .unwrap();
    let mut body = [0; 128];
    assert!(
        timeout(Duration::from_millis(30), socket.recv_from(&mut body))
            .await
            .is_err()
    );
    relay
        .send_to(&frame(media(), b"valid"), destination)
        .await
        .unwrap();
    let (n, from) = timeout(Duration::from_secs(1), socket.recv_from(&mut body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&body[..n], b"valid");
    assert_eq!(from, media());
}

#[tokio::test]
async fn mtu_accounting_includes_socks_address_and_udp_ip_overhead() {
    for server in [
        media(),
        SocketAddr::new("2606:4700:4700::1111".parse().unwrap(), 50001),
    ] {
        let (_context, socket, relay, _control) = ready(server, false, false).await;
        let maximum = socket.max_payload();
        assert_eq!(
            socket.send_to(&vec![7; maximum], server).await.unwrap(),
            maximum
        );
        let mut packet = [0; 2048];
        let (n, _) = relay.recv_from(&mut packet).await.unwrap();
        assert_eq!(n, OUTER_IPV4_PAYLOAD);
        assert_eq!(n + 8 + 20, 1500);
        assert_eq!(
            socket
                .send_to(&vec![0; maximum + 1], server)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            socket
                .send_to(b"x", "8.8.8.8:50001".parse().unwrap())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(
            timeout(Duration::from_millis(20), relay.recv_from(&mut packet))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn control_eof_or_unexpected_data_stops_both_sending_and_receiving() {
    for unexpected_data in [false, true] {
        let (_context, socket, relay, mut control) = ready(media(), false, false).await;
        if unexpected_data {
            control.write_all(b"unexpected").await.unwrap();
        } else {
            control.shutdown().await.unwrap();
        }
        let mut body = [0; 128];
        let error = timeout(Duration::from_secs(1), socket.recv_from(&mut body))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionAborted);
        assert_eq!(
            error.get_ref().unwrap().downcast_ref::<NetworkError>(),
            Some(&NetworkError::ProxyUdpClosed)
        );
        assert_eq!(
            socket
                .send_to(b"private-media", media())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        assert!(
            timeout(Duration::from_millis(20), relay.recv_from(&mut body))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn eight_associations_are_shared_across_context_views_and_drop_releases_the_slot() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let bound = relay.local_addr().unwrap();
    let context = NetworkContext::off()
        .with_proxy(
            config(listener.local_addr().unwrap(), false),
            Zeroizing::new(String::new()),
        )
        .unwrap();
    let fixture = tokio::spawn(async move {
        let mut held = Vec::new();
        for _ in 0..MAX_ASSOCIATIONS + 1 {
            let (mut control, _) = listener.accept().await.unwrap();
            associate_request(&mut control, false).await;
            reply(&mut control, bound).await;
            held.push(control);
        }
        held
    });
    let mut sockets = Vec::new();
    for _ in 0..MAX_ASSOCIATIONS {
        sockets.push(context.bind_udp(media()).await.unwrap());
    }
    let view = context
        .with_mode(crate::settings::Mode::Off)
        .with_mode(crate::settings::Mode::Proxy)
        .with_experimental_udp(true);
    let error = view.bind_udp(media()).await.unwrap_err();
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<NetworkError>(),
        Some(&NetworkError::BudgetExhausted)
    );
    drop(sockets.pop());
    sockets.push(view.bind_udp(media()).await.unwrap());
    let mut controls = fixture.await.unwrap();
    drop(sockets);
    let mut byte = [0];
    for control in &mut controls {
        assert_eq!(
            timeout(Duration::from_secs(1), control.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn proxy_failure_and_unsupported_proxy_never_return_a_direct_media_socket() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    for kind in [ProxyKind::Http, ProxyKind::Https, ProxyKind::Socks5] {
        let context = NetworkContext::off()
            .with_proxy(
                ProxyConfig {
                    kind,
                    ..config(address, false)
                },
                Zeroizing::new(String::new()),
            )
            .unwrap();
        assert_eq!(context.media_supported(), kind == ProxyKind::Socks5);
        assert!(
            timeout(Duration::from_secs(12), context.bind_udp(media()))
                .await
                .unwrap()
                .is_err()
        );
        assert!(
            context
                .bind_udp("127.0.0.1:50001".parse().unwrap())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn cancel_during_association_auth_closes_control_without_falling_back() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let context = NetworkContext::off()
        .with_proxy(
            config(listener.local_addr().unwrap(), false),
            Zeroizing::new(String::new()),
        )
        .unwrap();
    let task = tokio::spawn(async move { context.bind_udp(media()).await });
    let (mut control, _) = listener.accept().await.unwrap();
    let mut hello = [0; 3];
    control.read_exact(&mut hello).await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(1), control.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn relay_authority_checks_wildcards_family_port_and_unsafe_addresses() {
    let peer: SocketAddr = "127.0.0.1:1080".parse().unwrap();
    assert_eq!(
        relay_address(
            SocksBound {
                host: SocksHost::Ip(Ipv4Addr::UNSPECIFIED.into()),
                port: 5000
            },
            peer
        )
        .await
        .unwrap(),
        "127.0.0.1:5000".parse().unwrap()
    );
    for ip in ["224.0.0.1", "255.255.255.255", "169.254.1.2", "::1"] {
        assert!(
            relay_address(
                SocksBound {
                    host: SocksHost::Ip(ip.parse().unwrap()),
                    port: 5000
                },
                peer
            )
            .await
            .is_err()
        );
    }
    assert!(
        relay_address(
            SocksBound {
                host: SocksHost::Ip(peer.ip()),
                port: 0
            },
            peer
        )
        .await
        .is_err()
    );
    assert!(!valid_relay(
        "10.0.0.1:5000".parse().unwrap(),
        "8.8.8.8:1080".parse().unwrap()
    ));
    assert_eq!(
        relay_address(
            SocksBound {
                host: SocksHost::Domain("localhost".into()),
                port: 5000
            },
            peer
        )
        .await
        .unwrap(),
        "127.0.0.1:5000".parse().unwrap()
    );
    assert!(
        relay_address(
            SocksBound {
                host: SocksHost::Domain("localhost".into()),
                port: 5000
            },
            "8.8.8.8:1080".parse().unwrap()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn association_deadlines_and_malformed_reply_refuse_without_a_direct_socket() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let context = NetworkContext::off()
        .with_proxy(
            config(listener.local_addr().unwrap(), false),
            Zeroizing::new(String::new()),
        )
        .unwrap();
    let task = tokio::spawn(async move { context.bind_udp(media()).await });
    let (mut control, _) = listener.accept().await.unwrap();
    let mut hello = [0; 3];
    control.read_exact(&mut hello).await.unwrap();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(20)).await;
    let error = task.await.unwrap().unwrap_err();
    assert_eq!(
        error.get_ref().unwrap().downcast_ref::<NetworkError>(),
        Some(&NetworkError::ProxyTimeout)
    );
    tokio::time::resume();
    let mut byte = [0];
    assert_eq!(
        timeout(Duration::from_secs(1), control.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    for reply_bytes in [
        vec![5, 7, 0, 1, 127, 0, 0, 1, 1, 1],
        vec![5, 0, 1, 1, 127, 0, 0, 1, 1, 1],
        vec![5, 0, 0, 3, 0],
    ] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let context = NetworkContext::off()
            .with_proxy(
                config(listener.local_addr().unwrap(), false),
                Zeroizing::new(String::new()),
            )
            .unwrap();
        let fixture = tokio::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();
            associate_request(&mut control, false).await;
            control.write_all(&reply_bytes).await.unwrap();
        });
        assert!(context.bind_udp(media()).await.is_err());
        fixture.await.unwrap();
    }
}
