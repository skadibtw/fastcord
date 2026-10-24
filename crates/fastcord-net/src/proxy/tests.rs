//! Local proxy fixtures. The origin names deliberately do not resolve: only
//! the configured proxy can carry the transport, so a direct fallback fails.

use super::*;
use base64::Engine;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn credentials(address: SocketAddr, kind: ProxyKind, authenticated: bool) -> ProxyCredentials {
    ProxyCredentials::new(
        ProxyConfig {
            kind,
            host: address.ip().to_string(),
            port: address.port(),
            username: authenticated.then(|| "fixture-user".to_owned()),
        },
        Zeroizing::new(
            if authenticated {
                "fixture-password"
            } else {
                ""
            }
            .to_owned(),
        ),
    )
    .unwrap()
}

#[test]
fn proxy_configuration_credentials_are_bounded_redacted_and_not_serialized() {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, 9876));
    let saved = credentials(address, ProxyKind::Http, true);
    let serialized = serde_json::to_string(&saved.config).unwrap();
    assert!(!serialized.contains("fixture-password"));
    let diagnostic = format!("{saved:?}");
    for secret in ["fixture-password", "fixture-user", "127.0.0.1"] {
        assert!(!diagnostic.contains(secret));
    }
    for bad in [
        "http://proxy.example",
        "user@proxy.example",
        "proxy.example/path",
        "proxy\n.example",
        "[[::1]]",
        "[::1",
        "::1]",
    ] {
        let mut config = saved.config.clone();
        config.host = bad.to_owned();
        assert!(
            config.validate().is_err(),
            "invalid proxy host was accepted"
        );
    }
    let mut config = saved.config.clone();
    config.username = Some("x".repeat(MAX_CREDENTIAL_BYTES + 1));
    assert!(ProxyCredentials::new(config, Zeroizing::new("password".to_owned())).is_err());
    assert!(
        ProxyCredentials::new(
            saved.config.clone(),
            Zeroizing::new("x".repeat(MAX_CREDENTIAL_BYTES + 1))
        )
        .is_err()
    );
    let mut document = serde_json::to_value(&saved.config).unwrap();
    document["password"] = serde_json::Value::String("must-not-be-stored".to_owned());
    assert!(serde_json::from_value::<ProxyConfig>(document).is_err());
}

async fn head<S: tokio::io::AsyncRead + Unpin>(socket: &mut S) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= 8 * 1024);
        if bytes.ends_with(b"\r\n\r\n") {
            return bytes;
        }
    }
}

async fn greeting(socket: &mut TcpStream, authenticated: bool) {
    let mut prefix = [0; 2];
    socket.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix[0], 5);
    assert!((1..=2).contains(&prefix[1]));
    let mut methods = vec![0; usize::from(prefix[1])];
    socket.read_exact(&mut methods).await.unwrap();
    let selected = if authenticated { 2 } else { 0 };
    assert!(methods.contains(&selected));
    socket.write_all(&[5, selected]).await.unwrap();
    if authenticated {
        let mut auth = [0; 2];
        socket.read_exact(&mut auth).await.unwrap();
        assert_eq!(auth[0], 1);
        let mut user = vec![0; usize::from(auth[1])];
        socket.read_exact(&mut user).await.unwrap();
        let length = socket.read_u8().await.unwrap();
        let mut password = vec![0; usize::from(length)];
        socket.read_exact(&mut password).await.unwrap();
        assert!(user == b"fixture-user");
        assert!(password == b"fixture-password");
        socket.write_all(&[1, 0]).await.unwrap();
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Destination {
    V4([u8; 4]),
    V6([u8; 16]),
    Domain(Vec<u8>),
}

async fn destination(socket: &mut TcpStream) -> (Destination, u16) {
    let mut prefix = [0; 4];
    socket.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix[..3], [5, 1, 0]);
    let target = match prefix[3] {
        1 => {
            let mut address = [0; 4];
            socket.read_exact(&mut address).await.unwrap();
            Destination::V4(address)
        }
        4 => {
            let mut address = [0; 16];
            socket.read_exact(&mut address).await.unwrap();
            Destination::V6(address)
        }
        3 => {
            let length = socket.read_u8().await.unwrap();
            let mut name = vec![0; usize::from(length)];
            socket.read_exact(&mut name).await.unwrap();
            Destination::Domain(name)
        }
        _ => panic!("invalid SOCKS5 destination address type"),
    };
    (target, socket.read_u16().await.unwrap())
}

#[tokio::test]
async fn socks5_remote_dns_and_authenticated_transport_use_only_the_proxy() {
    for authenticated in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(
            listener.local_addr().unwrap(),
            ProxyKind::Socks5,
            authenticated,
        );
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            greeting(&mut socket, authenticated).await;
            let (target, port) = destination(&mut socket).await;
            assert_eq!(
                target,
                Destination::Domain(b"only-the-proxy-can-resolve.invalid".to_vec())
            );
            assert_eq!(port, 443);
            // Fragment every reply byte; neither a read nor write is a TCP
            // segment assertion. The client must reassemble the wire fields.
            for byte in [5, 0, 0, 1, 0, 0, 0, 0, 0x12, 0x34] {
                socket.write_all(&[byte]).await.unwrap();
                tokio::task::yield_now().await;
            }
            let mut payload = [0; 9];
            socket.read_exact(&mut payload).await.unwrap();
            assert_eq!(&payload, b"origin-io");
            socket.write_all(b"response").await.unwrap();
        });
        let mut transport = tokio::time::timeout(
            Duration::from_secs(5),
            dial(&proxy, "only-the-proxy-can-resolve.invalid", 443),
        )
        .await
        .unwrap()
        .unwrap();
        transport.write_all(b"origin-io").await.unwrap();
        let mut response = [0; 8];
        transport.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"response");
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn socks5_ipv4_and_ipv6_origin_address_types() {
    numeric_destinations(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST)).await;
}

#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "requires unrestricted IPv6 loopback; maintainer host denies ::1 with Windows 10013"
)]
async fn socks5_ipv6_proxy_listener_roundtrip() {
    numeric_destinations(std::net::IpAddr::V6(Ipv6Addr::LOCALHOST)).await;
}

async fn numeric_destinations(proxy_ip: std::net::IpAddr) {
    for host in ["1.1.1.1", "2606:4700:4700::1111"] {
        let listener = TcpListener::bind((proxy_ip, 0)).await.unwrap();
        let proxy = credentials(listener.local_addr().unwrap(), ProxyKind::Socks5, false);
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            greeting(&mut socket, false).await;
            let (target, port) = destination(&mut socket).await;
            let expected = match host.parse::<std::net::IpAddr>().unwrap() {
                std::net::IpAddr::V4(address) => Destination::V4(address.octets()),
                std::net::IpAddr::V6(address) => Destination::V6(address.octets()),
            };
            assert_eq!(target, expected);
            assert_eq!(port, 443);
            // BND.ADDR may use the opposite family to the origin.
            let mut reply = vec![5, 0, 0, 4];
            reply.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
            reply.extend_from_slice(&9999u16.to_be_bytes());
            socket.write_all(&reply).await.unwrap();
            socket.write_all(b"ok").await.unwrap();
        });
        let mut transport = tokio::time::timeout(Duration::from_secs(5), dial(&proxy, host, 443))
            .await
            .unwrap()
            .unwrap_or_else(|error| panic!("proxy family {proxy_ip}, origin {host}: {error:?}"));
        let mut response = [0; 2];
        transport.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"ok");
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn socks5_refusals_and_malformed_fields_fail_closed() {
    let replies: &[&[u8]] = &[
        &[4, 0, 0, 1, 0, 0, 0, 0, 0, 0], // Wrong version.
        &[5, 5, 0, 1, 0, 0, 0, 0, 0, 0], // Refused.
        &[5, 0, 1, 1, 0, 0, 0, 0, 0, 0], // Nonzero reserved byte.
        &[5, 0, 0, 9],                   // Unknown address type.
        &[5, 0, 0, 3, 0, 0, 0],          // Empty bound domain.
        &[5, 0, 0, 4, 0],                // Truncated bound IPv6.
    ];
    for &reply in replies {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(listener.local_addr().unwrap(), ProxyKind::Socks5, false);
        let reply = reply.to_vec();
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            greeting(&mut socket, false).await;
            let _ = destination(&mut socket).await;
            socket.write_all(&reply).await.unwrap();
        });
        let result =
            tokio::time::timeout(Duration::from_secs(5), dial(&proxy, "example.invalid", 443))
                .await
                .unwrap();
        assert!(result.is_err());
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn socks5_method_and_password_rejection_do_not_retry_without_authentication() {
    for reject_password in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(listener.local_addr().unwrap(), ProxyKind::Socks5, true);
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut prefix = [0; 2];
            socket.read_exact(&mut prefix).await.unwrap();
            let mut methods = vec![0; usize::from(prefix[1])];
            socket.read_exact(&mut methods).await.unwrap();
            if reject_password {
                socket.write_all(&[5, 2]).await.unwrap();
                let mut auth = [0; 2];
                socket.read_exact(&mut auth).await.unwrap();
                let mut user = vec![0; usize::from(auth[1])];
                socket.read_exact(&mut user).await.unwrap();
                let count = socket.read_u8().await.unwrap();
                let mut password = vec![0; usize::from(count)];
                socket.read_exact(&mut password).await.unwrap();
                socket.write_all(&[1, 1]).await.unwrap();
            } else {
                socket.write_all(&[5, 0xff]).await.unwrap();
            }
        });
        let error =
            tokio::time::timeout(Duration::from_secs(5), dial(&proxy, "example.invalid", 443))
                .await
                .unwrap()
                .err()
                .unwrap();
        assert_eq!(error, NetworkError::ProxyAuthentication);
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn http_connect_auth_remote_authority_and_opaque_payload_are_separate() {
    for authenticated in [false, true] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(
            listener.local_addr().unwrap(),
            ProxyKind::Http,
            authenticated,
        );
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = head(&mut socket).await;
            let text = std::str::from_utf8(&request).unwrap();
            assert!(
                text.starts_with("CONNECT only-the-proxy-can-resolve.invalid:443 HTTP/1.1\r\n")
            );
            if authenticated {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode("fixture-user:fixture-password");
                assert!(text.lines().any(|line| {
                    line.eq_ignore_ascii_case(&format!("Proxy-Authorization: Basic {encoded}"))
                }));
            } else {
                assert!(!text.to_ascii_lowercase().contains("proxy-authorization"));
            }
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut opaque = [0; 9];
            socket.read_exact(&mut opaque).await.unwrap();
            assert_eq!(&opaque, b"origin-io");
            socket.write_all(b"response").await.unwrap();
        });
        let mut transport = tokio::time::timeout(
            Duration::from_secs(5),
            dial(&proxy, "only-the-proxy-can-resolve.invalid", 443),
        )
        .await
        .unwrap()
        .unwrap();
        transport.write_all(b"origin-io").await.unwrap();
        let mut response = [0; 8];
        transport.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"response");
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn http_connect_rejects_refusal_redirect_oversize_and_eof() {
    let mut oversized = b"HTTP/1.1 200 Connection Established\r\nX-Long: ".to_vec();
    oversized.extend(std::iter::repeat_n(b'x', 8 * 1024));
    oversized.extend_from_slice(b"\r\n\r\n");
    let replies = [
        b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n".to_vec(),
        b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/\r\n\r\n".to_vec(),
        b"HTTP/1.1 500 Error\r\n\r\n".to_vec(),
        b"not HTTP\r\n\r\n".to_vec(),
        b"HTTP/1.1 200".to_vec(),
        oversized,
    ];
    for reply in replies {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(listener.local_addr().unwrap(), ProxyKind::Http, false);
        let fixture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = head(&mut socket).await;
            let _ = socket.write_all(&reply).await;
        });
        let error =
            tokio::time::timeout(Duration::from_secs(5), dial(&proxy, "example.invalid", 443))
                .await
                .unwrap()
                .err()
                .unwrap();
        assert!(matches!(
            error,
            NetworkError::ProxyRefused
                | NetworkError::ProxyAuthentication
                | NetworkError::ProxyProtocol
        ));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("fixture-password"));
        fixture.await.unwrap();
    }
}

#[tokio::test]
async fn unavailable_proxy_never_connects_to_the_origin_directly() {
    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let temporary_proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy_address = temporary_proxy.local_addr().unwrap();
    drop(temporary_proxy);
    for kind in [ProxyKind::Http, ProxyKind::Https, ProxyKind::Socks5] {
        let proxy = credentials(proxy_address, kind, false);
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            dial(&proxy, "127.0.0.1", origin.local_addr().unwrap().port()),
        )
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), origin.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn proxy_handshake_deadlines_and_cancellation_close_the_socket() {
    for kind in [ProxyKind::Http, ProxyKind::Socks5] {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(listener.local_addr().unwrap(), kind, false);
        let task = tokio::spawn(async move { dial(&proxy, "example.invalid", 443).await });
        let (mut socket, _) = listener.accept().await.unwrap();
        if kind == ProxyKind::Http {
            let _ = head(&mut socket).await;
        } else {
            let mut hello = [0; 3];
            socket.read_exact(&mut hello).await.unwrap();
            assert_eq!(hello, [5, 1, 0]);
        }
        // Start virtual time only after real TCP establishment, avoiding a
        // race between virtual deadlines and the host's network reactor.
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60)).await;
        let result = tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err());
        tokio::time::resume();

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy = credentials(listener.local_addr().unwrap(), kind, false);
        let task = tokio::spawn(async move { dial(&proxy, "example.invalid", 443).await });
        let (mut socket, _) = listener.accept().await.unwrap();
        if kind == ProxyKind::Http {
            let _ = head(&mut socket).await;
        } else {
            let mut hello = [0; 3];
            socket.read_exact(&mut hello).await.unwrap();
        }
        task.abort();
        assert!(task.await.err().unwrap().is_cancelled());
        let mut trailing = [0];
        let closed = tokio::time::timeout(Duration::from_secs(1), socket.read(&mut trailing))
            .await
            .unwrap();
        assert!(matches!(closed, Ok(0) | Err(_)));
    }
}

#[tokio::test]
async fn every_http_role_and_websocket_role_routes_through_the_configured_proxy() {
    use crate::{NetworkContext, Role};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn host(role: Role) -> &'static str {
        match role {
            Role::Rest | Role::Login | Role::Profile => "discord.com",
            Role::Gateway => "gateway.discord.gg",
            Role::RemoteAuth => "remote-auth-gateway.discord.gg",
            Role::Voice => "voice-fixture.discord.media",
            Role::Stream => "stream-fixture.discord.media",
            Role::Cdn => "cdn.discordapp.com",
            Role::Upload => "only-the-proxy-can-resolve.invalid",
        }
    }

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let config = credentials(listener.local_addr().unwrap(), ProxyKind::Http, true).config;
    let context = NetworkContext::off()
        .with_proxy(config, Zeroizing::new("fixture-password".to_owned()))
        .unwrap();
    let connected = Arc::new(AtomicUsize::new(0));
    let observed = connected.clone();
    let fixture = tokio::spawn(async move {
        for _ in 0..13 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = head(&mut socket).await;
            let text = std::str::from_utf8(&request).unwrap();
            assert!(text.starts_with("CONNECT "));
            let encoded =
                base64::engine::general_purpose::STANDARD.encode("fixture-user:fixture-password");
            assert!(text.lines().any(|line| {
                line.eq_ignore_ascii_case(&format!("Proxy-Authorization: Basic {encoded}"))
            }));
            // The private bridge credential and origin's token must not be
            // forwarded as an upstream CONNECT field.
            assert_eq!(
                text.lines()
                    .filter(|line| line
                        .to_ascii_lowercase()
                        .starts_with("proxy-authorization:"))
                    .count(),
                1
            );
            assert!(!text.contains("origin-token-canary"));
            observed.fetch_add(1, Ordering::Relaxed);
            socket.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=fixture\r\n\r\n").await.unwrap();
        }
    });
    for role in Role::ALL {
        let client = context.http_builder(role).unwrap().build().unwrap();
        let mut request = client.get(format!("https://{}/required-normal-flow", host(role)));
        if role == Role::Rest {
            request = request.header(reqwest::header::AUTHORIZATION, "origin-token-canary");
        }
        let error = tokio::time::timeout(Duration::from_secs(5), request.send())
            .await
            .unwrap()
            .err()
            .unwrap();
        assert_eq!(
            error.category(),
            NetworkError::ProxyAuthentication,
            "role {role:?}"
        );
    }
    for role in [Role::Gateway, Role::RemoteAuth, Role::Voice, Role::Stream] {
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            context.connect_tls(host(role), 443, role),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(error, NetworkError::ProxyAuthentication, "role {role:?}");
    }
    fixture.await.unwrap();
    assert_eq!(connected.load(Ordering::Relaxed), 13);
    assert_eq!(context.controller().budget_snapshot().rolling_dials, 0);
}

#[tokio::test]
async fn proxy_cannot_replace_origin_certificate_checks_for_http_or_websocket() {
    use crate::{NetworkContext, Role};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls;

    let certificate =
        CertificateDer::from(include_bytes!("../tls/fixtures/test-cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("../tls/fixtures/test-key.der").to_vec(),
    ));
    let server = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate], key)
    .unwrap();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let config = credentials(listener.local_addr().unwrap(), ProxyKind::Http, false).config;
    let context = NetworkContext::off()
        .with_proxy(config, Zeroizing::new(String::new()))
        .unwrap();
    let fixture = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = head(&mut socket).await;
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            // The proxy presents a certificate its configured user has not
            // trusted. Neither origin client is allowed to accept this MITM.
            assert!(
                tokio_rustls::TlsAcceptor::from(Arc::new(server.clone()))
                    .accept(socket)
                    .await
                    .is_err()
            );
        }
    });
    let client = context.http_builder(Role::Cdn).unwrap().build().unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        client.get("https://cdn.discordapp.com/fixture").send(),
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    assert_eq!(error.category(), NetworkError::CertificateRejected);
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        context.connect_tls("gateway.discord.gg", 443, Role::Gateway),
    )
    .await
    .unwrap()
    .err()
    .unwrap();
    assert_eq!(error, NetworkError::CertificateRejected);
    fixture.await.unwrap();
    assert_eq!(context.controller().budget_snapshot().rolling_dials, 0);
}

#[tokio::test]
async fn websocket_upgrade_and_gateway_hello_cross_each_tcp_proxy_kind() {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio_rustls::rustls;

    let certificate =
        CertificateDer::from(include_bytes!("../tls/fixtures/test-cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("../tls/fixtures/test-key.der").to_vec(),
    ));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certificate.clone()).unwrap();
    let mut client_tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    client_tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let mut server_tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(vec![certificate], key)
    .unwrap();
    server_tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    let client_tls = Arc::new(client_tls);
    let server_tls = Arc::new(server_tls);

    for kind in [ProxyKind::Http, ProxyKind::Https, ProxyKind::Socks5] {
        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let proxy_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let origin_address = origin.local_addr().unwrap();
        let proxy = ProxyCredentials::new(
            ProxyConfig {
                kind,
                host: "localhost".to_owned(),
                port: proxy_listener.local_addr().unwrap().port(),
                username: Some("fixture-user".to_owned()),
            },
            Zeroizing::new("fixture-password".to_owned()),
        )
        .unwrap();
        let origin_tls = server_tls.clone();
        let origin_task = tokio::spawn(async move {
            let (socket, _) = origin.accept().await.unwrap();
            let mut tls = tokio_rustls::TlsAcceptor::from(origin_tls)
                .accept(socket)
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.server_name(), Some("localhost"));
            assert_eq!(
                tls.get_ref().1.alpn_protocol(),
                Some(b"http/1.1".as_slice())
            );
            let request = head(&mut tls).await;
            let request = std::str::from_utf8(&request).unwrap();
            assert!(request.starts_with("GET /gateway?v=10&encoding=json HTTP/1.1\r\n"));
            assert!(request.contains("Upgrade: websocket\r\n"));
            assert!(request.contains("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"));
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            assert!(!request.contains("fixture-password"));
            // The key/accept pair is RFC 6455's public example. HELLO is an
            // unmasked final server text frame, without an Identify or token.
            tls.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n").await.unwrap();
            let hello = br#"{"op":10,"d":{"heartbeat_interval":45000}}"#;
            tls.write_all(&[0x81, u8::try_from(hello.len()).unwrap()])
                .await
                .unwrap();
            tls.write_all(hello).await.unwrap();
            tls.shutdown().await.unwrap();
        });
        let outer_tls = server_tls.clone();
        let proxy_task = tokio::spawn(async move {
            let (mut socket, _) = proxy_listener.accept().await.unwrap();
            let mut transport: BoxedIo = if kind == ProxyKind::Socks5 {
                greeting(&mut socket, true).await;
                let (host, port) = destination(&mut socket).await;
                assert_eq!(host, Destination::Domain(b"localhost".to_vec()));
                assert_eq!(port, origin_address.port());
                socket
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();
                Box::new(socket)
            } else {
                let mut transport: BoxedIo = if kind == ProxyKind::Https {
                    Box::new(
                        tokio_rustls::TlsAcceptor::from(outer_tls)
                            .accept(socket)
                            .await
                            .unwrap(),
                    )
                } else {
                    Box::new(socket)
                };
                let request = head(&mut transport).await;
                let request = std::str::from_utf8(&request).unwrap();
                assert!(request.starts_with(&format!(
                    "CONNECT localhost:{} HTTP/1.1\r\n",
                    origin_address.port()
                )));
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode("fixture-user:fixture-password");
                assert!(request.contains(&format!("Proxy-Authorization: Basic {encoded}\r\n")));
                transport
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();
                transport
            };
            let mut upstream = TcpStream::connect(origin_address).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut transport, &mut upstream).await;
        });
        let check = async {
            let transport = dial_with_tls(
                &proxy,
                "localhost",
                origin_address.port(),
                client_tls.clone(),
            )
            .await
            .unwrap();
            let mut tls = TlsConnector::from(client_tls.clone())
                .connect(ServerName::try_from("localhost").unwrap(), transport)
                .await
                .unwrap();
            tls.write_all(format!("GET /gateway?v=10&encoding=json HTTP/1.1\r\nHost: localhost:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n", origin_address.port()).as_bytes()).await.unwrap();
            tls.flush().await.unwrap();
            let response = head(&mut tls).await;
            let response = std::str::from_utf8(&response).unwrap();
            assert!(response.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
            assert!(response.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
            let mut frame = [0; 2];
            tls.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame[0], 0x81);
            assert!(frame[1] < 126);
            let mut payload = vec![0; usize::from(frame[1])];
            tls.read_exact(&mut payload).await.unwrap();
            let hello: serde_json::Value = serde_json::from_slice(&payload).unwrap();
            assert_eq!(hello["op"], 10);
            assert_eq!(hello["d"]["heartbeat_interval"], 45000);
            tls.shutdown().await.unwrap();
            drop(tls);
            origin_task.await.unwrap();
            proxy_task.await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(10), check)
            .await
            .unwrap();
    }
}
