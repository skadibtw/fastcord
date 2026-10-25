//! End-to-end local fixtures. Test certificates are trusted only in these
//! tests; production origin and HTTPS proxy validation always uses webpki.

use super::*;
use crate::{NetworkContext, Role};
use std::net::Ipv4Addr;
use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{RootCertStore, ServerConfig, crypto};

fn tls(alpn: &[&[u8]]) -> (ClientConfig, ServerConfig) {
    let certificate =
        CertificateDer::from(include_bytes!("../tls/fixtures/test-cert.der").to_vec());
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        include_bytes!("../tls/fixtures/test-key.der").to_vec(),
    ));
    let mut roots = RootCertStore::empty();
    roots.add(certificate.clone()).unwrap();
    let mut client =
        ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
    client.alpn_protocols = alpn.iter().map(|value| value.to_vec()).collect();
    let mut server =
        ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .unwrap();
    server.alpn_protocols = client.alpn_protocols.clone();
    (client, server)
}

async fn head<S: AsyncRead + Unpin>(io: &mut S) -> Vec<u8> {
    let mut buffer = Vec::new();
    while !buffer.ends_with(b"\r\n\r\n") {
        buffer.push(io.read_u8().await.unwrap());
        assert!(buffer.len() <= MAX_HEADERS);
    }
    buffer
}

fn credentials(port: u16, kind: ProxyKind) -> ProxyCredentials {
    ProxyCredentials::new(
        ProxyConfig {
            kind,
            host: "localhost".into(),
            port,
            username: Some("fixture-user".into()),
        },
        Zeroizing::new("fixture-password".into()),
    )
    .unwrap()
}

#[test]
fn proxy_metadata_keys_credentials_and_settings_stay_bounded_and_redacted() {
    let base = ProxyConfig {
        kind: ProxyKind::Http,
        host: "Proxy.Example".into(),
        port: 8080,
        username: Some("fixture-user".into()),
    };
    let mut canonical = base.clone();
    canonical.host = "proxy.example".into();
    assert_eq!(base.credential_key(), canonical.credential_key());
    assert_eq!(base.credential_key().len(), 64);
    for changed in [
        ProxyConfig {
            port: 8081,
            ..base.clone()
        },
        ProxyConfig {
            kind: ProxyKind::Https,
            ..base.clone()
        },
        ProxyConfig {
            username: Some("different".into()),
            ..base.clone()
        },
    ] {
        assert_ne!(base.credential_key(), changed.credential_key());
    }
    for bad in [
        ProxyConfig {
            host: "[[::1]]".into(),
            ..base.clone()
        },
        ProxyConfig {
            host: "https://proxy.example".into(),
            ..base.clone()
        },
        ProxyConfig {
            username: Some("bad\r\nuser".into()),
            ..base.clone()
        },
        ProxyConfig {
            username: Some("bad:user".into()),
            ..base.clone()
        },
        ProxyConfig {
            username: Some("u".repeat(256)),
            ..base.clone()
        },
    ] {
        assert!(bad.validate().is_err());
    }
    assert!(ProxyCredentials::new(base.clone(), Zeroizing::new("p".repeat(256))).is_err());
    assert!(
        ProxyCredentials::new(
            ProxyConfig {
                username: None,
                ..base.clone()
            },
            Zeroizing::new("unexpected".into())
        )
        .is_err()
    );
    let credentials =
        ProxyCredentials::new(base.clone(), Zeroizing::new("secret-canary".into())).unwrap();
    assert!(!format!("{credentials:?}").contains("secret-canary"));
    assert!(!format!("{base:?}").contains("fixture-user"));
    let settings = crate::settings::Settings {
        proxy: Some(base),
        ..Default::default()
    };
    let bytes = settings.encode(0).unwrap();
    assert!(
        !std::str::from_utf8(&bytes)
            .unwrap()
            .contains("secret-canary")
    );
    assert_eq!(
        crate::settings::Settings::decode(&bytes, 0).unwrap().proxy,
        settings.proxy
    );
    let legacy = br#"{"version":1,"tls_profile_version":1,"catalog_version":1,"mode":"Off","experimental_udp":false,"profiles":[]}"#;
    assert!(
        crate::settings::Settings::decode(legacy, 0)
            .unwrap()
            .proxy
            .is_none()
    );
}

#[tokio::test]
async fn https_connect_validates_proxy_certificate_and_preserves_nested_origin_tls() {
    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let credentials = credentials(proxy.local_addr().unwrap().port(), ProxyKind::Https);
    let (proxy_client, proxy_server) = tls(&[]);
    let (origin_client, origin_server) = tls(&[b"h2"]);
    let fixture = tokio::spawn(async move {
        // First connection deliberately uses production roots and must fail.
        let (first, _) = proxy.accept().await.unwrap();
        assert!(
            tokio_rustls::TlsAcceptor::from(Arc::new(proxy_server.clone()))
                .accept(first)
                .await
                .is_err()
        );
        let (socket, _) = proxy.accept().await.unwrap();
        let mut socket = tokio_rustls::TlsAcceptor::from(Arc::new(proxy_server))
            .accept(socket)
            .await
            .unwrap();
        let request = head(&mut socket).await;
        let request = std::str::from_utf8(&request).unwrap();
        assert!(request.starts_with(&format!("CONNECT localhost:{origin_port} HTTP/1.1\r\n")));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        let mut upstream = TcpStream::connect(origin.local_addr().unwrap())
            .await
            .unwrap();
        let (origin_socket, _) = origin.accept().await.unwrap();
        let origin = tokio::spawn(async move {
            let mut io = tokio_rustls::TlsAcceptor::from(Arc::new(origin_server))
                .accept(origin_socket)
                .await
                .unwrap();
            assert_eq!(io.get_ref().1.server_name(), Some("localhost"));
            assert_eq!(io.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            let mut body = [0; 16];
            io.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"end-to-end bytes");
            io.write_all(b"origin-reply").await.unwrap();
            io.shutdown().await.unwrap();
        });
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        origin.await.unwrap();
    });
    let check = async {
        assert!(matches!(
            dial(&credentials, "localhost", origin_port).await,
            Err(NetworkError::ProxyCertificateRejected)
        ));
        let transport = dial_with_tls(
            &credentials,
            "localhost",
            origin_port,
            Arc::new(proxy_client),
        )
        .await
        .unwrap();
        let mut origin = TlsConnector::from(Arc::new(origin_client))
            .connect(ServerName::try_from("localhost").unwrap(), transport)
            .await
            .unwrap();
        assert_eq!(origin.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        origin.write_all(b"end-to-end bytes").await.unwrap();
        origin.flush().await.unwrap();
        let mut reply = [0; 12];
        origin.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"origin-reply");
        origin.shutdown().await.unwrap();
        drop(origin);
        fixture.await.unwrap();
    };
    timeout(Duration::from_secs(10), check).await.unwrap();
}

#[tokio::test]
async fn http_factory_preserves_h2_through_both_connect_hops() {
    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let credentials = credentials(proxy.local_addr().unwrap().port(), ProxyKind::Http);
    let (client_tls, origin_tls) = tls(&[b"h2"]);
    let config = credentials.config.clone();
    let context = NetworkContext::off()
        .with_proxy(config, Zeroizing::new("fixture-password".into()))
        .unwrap();
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = proxy.accept().await.unwrap();
        let request = head(&mut socket).await;
        assert!(
            request.starts_with(format!("CONNECT localhost:{origin_port} HTTP/1.1\r\n").as_bytes())
        );
        let mut upstream = TcpStream::connect(origin.local_addr().unwrap())
            .await
            .unwrap();
        let (origin_socket, _) = origin.accept().await.unwrap();
        let origin = tokio::spawn(async move {
            let mut io = tokio_rustls::TlsAcceptor::from(Arc::new(origin_tls))
                .accept(origin_socket)
                .await
                .unwrap();
            assert_eq!(io.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            let mut preface = [0; 24];
            io.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            io.write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0]).await.unwrap();
            io.flush().await.unwrap();
            loop {
                let mut header = [0; 9];
                io.read_exact(&mut header).await.unwrap();
                let len = (usize::from(header[0]) << 16)
                    | (usize::from(header[1]) << 8)
                    | usize::from(header[2]);
                assert!(len <= 16 * 1024);
                let mut payload = vec![0; len];
                io.read_exact(&mut payload).await.unwrap();
                if header[3] == 1 {
                    assert_eq!(&header[5..], &[0, 0, 0, 1]);
                    break;
                }
                if header[3] == 4 && header[4] & 1 == 0 {
                    io.write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0]).await.unwrap();
                    io.flush().await.unwrap();
                }
            }
            io.write_all(&[
                0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88, 0, 0, 4, 0, 1, 0, 0, 0, 1, b'o', b'k', b'a', b'y',
            ])
            .await
            .unwrap();
            io.flush().await.unwrap();
            io.shutdown().await.unwrap();
        });
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        origin.await.unwrap();
    });
    let check = async {
        let client = context
            .http_builder(Role::Cdn)
            .unwrap()
            .fixture_tls(client_tls)
            .build()
            .unwrap();
        let response = client
            .get(format!("https://localhost:{origin_port}/fixture"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.version(), reqwest::Version::HTTP_2);
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"okay");
        drop(client);
        drop(context);
        fixture.await.unwrap();
    };
    timeout(Duration::from_secs(10), check).await.unwrap();
}

#[tokio::test]
async fn upload_body_redirect_and_proxy_credentials_stay_on_their_intended_hops() {
    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let origin_port = origin.local_addr().unwrap().port();
    let credentials = credentials(proxy.local_addr().unwrap().port(), ProxyKind::Http);
    let context = NetworkContext::off()
        .with_proxy(
            credentials.config,
            Zeroizing::new("fixture-password".into()),
        )
        .unwrap();
    let (client_tls, server_tls) = tls(&[b"http/1.1"]);
    let fixture = tokio::spawn(async move {
        let (mut socket, _) = proxy.accept().await.unwrap();
        let request = head(&mut socket).await;
        assert!(
            String::from_utf8(request)
                .unwrap()
                .to_ascii_lowercase()
                .contains("proxy-authorization: basic ")
        );
        let mut upstream = TcpStream::connect(origin.local_addr().unwrap())
            .await
            .unwrap();
        let (origin_socket, _) = origin.accept().await.unwrap();
        let origin = tokio::spawn(async move {
            let mut io = tokio_rustls::TlsAcceptor::from(Arc::new(server_tls))
                .accept(origin_socket)
                .await
                .unwrap();
            let request = head(&mut io).await;
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("PUT /upload HTTP/1.1\r\n"));
            assert!(!request.to_ascii_lowercase().contains("proxy-authorization"));
            assert!(!request.contains("fixture-password"));
            let mut body = [0; 12];
            io.read_exact(&mut body).await.unwrap();
            assert_eq!(&body, b"upload-bytes");
            io.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://example.invalid/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            io.shutdown().await.unwrap();
        });
        socket
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        origin.await.unwrap();
    });
    let check = async {
        let client = context
            .http_builder(Role::Upload)
            .unwrap()
            .fixture_tls(client_tls)
            .build()
            .unwrap();
        let response = client
            .put(format!("https://localhost:{origin_port}/upload"))
            .body("upload-bytes")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        drop(response);
        drop(client);
        drop(context);
        fixture.await.unwrap();
    };
    timeout(Duration::from_secs(10), check).await.unwrap();
}

#[tokio::test]
async fn simultaneous_http_connections_keep_proxy_authentication_and_origin_certificate_errors_separate()
 {
    let proxy = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let context = NetworkContext::off()
        .with_proxy(
            ProxyConfig {
                kind: ProxyKind::Http,
                host: "127.0.0.1".into(),
                port: proxy.local_addr().unwrap().port(),
                username: None,
            },
            Zeroizing::new(String::new()),
        )
        .unwrap();
    let (_, server) = tls(&[]);
    let fixture = tokio::spawn(async move {
        let (mut a, _) = proxy.accept().await.unwrap();
        let a_head = head(&mut a).await;
        let (mut b, _) = proxy.accept().await.unwrap();
        let b_head = head(&mut b).await;
        for headers in [a_head, b_head] {
            let headers = String::from_utf8(headers).unwrap();
            assert!(headers.starts_with("CONNECT localhost:443 HTTP/1.1"));
            assert!(!headers.contains("fastcord-") && !headers.contains("canary"));
            assert!(!headers.to_ascii_lowercase().contains("proxy-authorization"));
        }
        b.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        let reject_certificate = tokio::spawn(async move {
            assert!(
                tokio_rustls::TlsAcceptor::from(Arc::new(server))
                    .accept(b)
                    .await
                    .is_err()
            );
        });
        a.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .await
            .unwrap();
        reject_certificate.await.unwrap();
    });
    let client = context
        .http_builder(Role::Cdn)
        .unwrap()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let check = async {
        let (a, b) = tokio::join!(
            client.get("https://localhost/same?canary=one").send(),
            client.get("https://localhost/same?canary=two").send()
        );
        let errors = [a.unwrap_err(), b.unwrap_err()];
        assert!(
            errors
                .iter()
                .any(|e| e.category() == NetworkError::ProxyAuthentication),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.category() == NetworkError::CertificateRejected),
            "{errors:?}"
        );
        assert!(!format!("{errors:?}").contains("canary"));
        fixture.await.unwrap();
    };
    timeout(Duration::from_secs(5), check).await.unwrap();
}
