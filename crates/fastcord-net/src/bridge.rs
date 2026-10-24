//! A private, authenticated CONNECT bridge. It forwards opaque end-to-end TLS;
//! there is no CA, TLS termination, request replay or direct fallback.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use rand_core::{OsRng, RngCore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, oneshot, watch};
use zeroize::Zeroizing;

use crate::broker::{FailureReporter, authorize, dial_public};
use crate::selector::{AddressFamily, Controller, DiagnosticTarget, TlsProfile};
use crate::tls::{ShapedStream, Strategy};
use crate::{NetworkContext, NetworkError, Role};

pub(crate) const HEADER_LIMIT: usize = 8 * 1024;
pub(crate) const TUNNEL_LIMIT: usize = 32;
pub(crate) const FORWARD_BUFFER: usize = 32 * 1024;
const HEADER_TIMEOUT: Duration = Duration::from_secs(3);

pub(crate) struct Bridge {
    address: SocketAddr,
    secret: Zeroizing<String>,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Bridge {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

impl Bridge {
    pub(crate) fn start(context: NetworkContext) -> Result<Self, NetworkError> {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| NetworkError::BridgeUnavailable)?;
        listener
            .set_nonblocking(true)
            .map_err(|_| NetworkError::BridgeUnavailable)?;
        let address = listener
            .local_addr()
            .map_err(|_| NetworkError::BridgeUnavailable)?;
        let mut random = Zeroizing::new([0u8; 32]);
        OsRng
            .try_fill_bytes(random.as_mut())
            .map_err(|_| NetworkError::BridgeUnavailable)?;
        let secret = Zeroizing::new(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random.as_ref()),
        );
        let credentials: Arc<Vec<(Role, Zeroizing<String>)>> = Arc::new(
            Role::ALL
                .into_iter()
                .map(|role| {
                    let raw =
                        Zeroizing::new(format!("fastcord-{}:{}", role.label(), secret.as_str()));
                    let value = format!(
                        "Basic {}",
                        base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
                    );
                    (role, Zeroizing::new(value))
                })
                .collect(),
        );
        let controller = context.controller().clone();
        let transforms = context.transforms();
        let slots = context.tunnel_slots();
        let reporter = context.failure_reporter();
        let (stop, receiver) = oneshot::channel();
        // No reference to NetworkContext is retained by the bridge task: doing
        // so would form a cycle and keep the listener alive after app teardown.
        std::thread::Builder::new()
            .name("fastcord-connect".into())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                runtime.block_on(async move {
                    let Ok(listener) = TcpListener::from_std(listener) else {
                        return;
                    };
                    run(
                        listener,
                        credentials,
                        controller,
                        transforms,
                        slots,
                        reporter,
                        receiver,
                    )
                    .await;
                });
            })
            .map_err(|_| NetworkError::BridgeUnavailable)?;
        Ok(Self {
            address,
            secret,
            stop: Some(stop),
        })
    }

    pub(crate) fn proxy(&self, role: Role) -> Result<reqwest::Proxy, NetworkError> {
        reqwest::Proxy::https(format!("http://{}", self.address))
            .map(|proxy| {
                proxy.basic_auth(&format!("fastcord-{}", role.label()), self.secret.as_str())
            })
            .map_err(|_| NetworkError::BridgeUnavailable)
    }
}

async fn run(
    listener: TcpListener,
    credentials: Arc<Vec<(Role, Zeroizing<String>)>>,
    controller: Controller,
    transforms: Arc<Semaphore>,
    slots: Arc<Semaphore>,
    reporter: FailureReporter,
    mut stop: oneshot::Receiver<()>,
) {
    let (closed, _) = watch::channel(false);
    loop {
        let accepted = tokio::select! {
            _ = &mut stop => break,
            accepted = listener.accept() => accepted,
        };
        let Ok((client, source)) = accepted else {
            break;
        };
        if !source.ip().is_loopback() {
            continue;
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let credentials = credentials.clone();
        let controller = controller.clone();
        let transforms = transforms.clone();
        let reporter = reporter.clone();
        let mut closing = closed.subscribe();
        tokio::spawn(async move {
            let _slot = slot;
            tokio::select! {
                _ = closing.changed() => {},
                _ = tunnel(client, &credentials, &controller, transforms, reporter) => {},
            }
        });
    }
    let _ = closed.send(true);
}

async fn tunnel(
    mut client: TcpStream,
    credentials: &[(Role, Zeroizing<String>)],
    controller: &Controller,
    transforms: Arc<Semaphore>,
    reporter: FailureReporter,
) {
    let headers = tokio::time::timeout(HEADER_TIMEOUT, read_headers(&mut client)).await;
    let Ok(Ok(headers)) = headers else {
        let _ = client
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let parsed = parse_connect(&headers, credentials);
    let Ok((host, port, role)) = parsed else {
        let _ = client
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    drop(headers);
    if authorize(role, &host, port).is_err() {
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }
    let mut failed_address = None;
    let (server, address) = match dial_public(&host, port, &mut failed_address).await {
        Ok(connected) => connected,
        Err(error) => {
            reporter.record(&host, port, role, TlsProfile::Http, error, failed_address);
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                .await;
            return;
        }
    };
    let family = if address.is_ipv4() {
        AddressFamily::V4
    } else {
        AddressFamily::V6
    };
    reporter.connected(&host, port, role, address);
    let strategy = if crate::broker::probe_authorized(role, &host) {
        DiagnosticTarget::new(host, port, role, family, TlsProfile::Http)
            .ok()
            .and_then(|target| controller.strategy_for(&target))
            .unwrap_or(Strategy::Direct)
    } else {
        Strategy::Direct
    };
    if client.set_nodelay(true).is_err()
        || client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
    {
        return;
    }
    // One immutable plan on one upstream. Partial transformed output cannot be
    // rolled back or replayed on another socket.
    let _ = forward(client, server, strategy, transforms).await;
}

async fn forward(
    client: TcpStream,
    server: TcpStream,
    strategy: Strategy,
    transforms: Arc<Semaphore>,
) -> std::io::Result<()> {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, server_write) = server.into_split();
    let mut server_write = ShapedStream::with_budget(server_write, strategy, transforms);
    let mut observer = server_write.server_hello_observer();
    let outgoing = async {
        let mut buffer = vec![0u8; FORWARD_BUFFER];
        loop {
            let len = client_read.read(&mut buffer).await?;
            if len == 0 {
                return server_write.shutdown().await;
            }
            server_write.write_all(&buffer[..len]).await?;
            // A network read can end anywhere in ClientHello. Unlike flush,
            // this does not turn an incomplete prefix into pass-through.
            server_write.drain_ready().await?;
        }
    };
    let incoming = async {
        let mut buffer = vec![0u8; FORWARD_BUFFER];
        loop {
            let len = server_read.read(&mut buffer).await?;
            if len == 0 {
                return client_write.shutdown().await;
            }
            observer.observe(&buffer[..len]);
            client_write.write_all(&buffer[..len]).await?;
        }
    };
    tokio::try_join!(outgoing, incoming)?;
    Ok(())
}

async fn read_headers(client: &mut TcpStream) -> std::io::Result<Zeroizing<Vec<u8>>> {
    let mut headers = Zeroizing::new(Vec::with_capacity(HEADER_LIMIT));
    let mut chunk = Zeroizing::new([0u8; 512]);
    loop {
        let room = HEADER_LIMIT - headers.len();
        if room == 0 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let length = client.read(&mut chunk[..room.min(512)]).await?;
        if length == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        headers.extend_from_slice(&chunk[..length]);
        if let Some(end) = headers.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            // CONNECT clients must wait for 200 before TLS. Reject smuggled
            // bodies/pipelined payload rather than losing or misparsing bytes.
            if end + 4 != headers.len() {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            return Ok(headers);
        }
    }
}

fn parse_connect(
    headers: &[u8],
    credentials: &[(Role, Zeroizing<String>)],
) -> Result<(String, u16, Role), NetworkError> {
    if headers.len() > HEADER_LIMIT || !headers.ends_with(b"\r\n\r\n") || headers.contains(&0) {
        return Err(NetworkError::InvalidTarget);
    }
    let text = std::str::from_utf8(headers).map_err(|_| NetworkError::InvalidTarget)?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().ok_or(NetworkError::InvalidTarget)?.split(' ');
    if first.next() != Some("CONNECT") {
        return Err(NetworkError::InvalidTarget);
    }
    let authority = first.next().ok_or(NetworkError::InvalidTarget)?;
    if first.next() != Some("HTTP/1.1") || first.next().is_some() {
        return Err(NetworkError::InvalidTarget);
    }
    let (host, port) = parse_authority(authority)?;
    let mut role = None;
    let mut host_seen = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(NetworkError::InvalidTarget);
        }
        let (key, value) = line.split_once(':').ok_or(NetworkError::InvalidTarget)?;
        if key.is_empty()
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(NetworkError::InvalidTarget);
        }
        let value = value.trim_matches([' ', '\t']);
        if key.eq_ignore_ascii_case("proxy-authorization") {
            if role.is_some() {
                return Err(NetworkError::ProxyRefused);
            }
            role = credentials.iter().find_map(|(role, expected)| {
                constant_time_eq(value.as_bytes(), expected.as_bytes()).then_some(*role)
            });
            if role.is_none() {
                return Err(NetworkError::ProxyRefused);
            }
        } else if key.eq_ignore_ascii_case("host") {
            if host_seen || value != authority {
                return Err(NetworkError::InvalidTarget);
            }
            host_seen = true;
        } else if key.eq_ignore_ascii_case("transfer-encoding")
            || (key.eq_ignore_ascii_case("content-length") && value != "0")
        {
            return Err(NetworkError::InvalidTarget);
        }
    }
    Ok((host, port, role.ok_or(NetworkError::ProxyRefused)?))
}

fn parse_authority(authority: &str) -> Result<(String, u16), NetworkError> {
    if authority.len() > 261 || authority.contains(['/', '\\', '@', '%', '?', '#']) {
        return Err(NetworkError::InvalidTarget);
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest.split_once(']').ok_or(NetworkError::InvalidTarget)?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(NetworkError::InvalidTarget);
        }
        (
            host,
            rest.strip_prefix(':').ok_or(NetworkError::InvalidTarget)?,
        )
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or(NetworkError::InvalidTarget)?;
        if !crate::broker::valid_host(host) {
            return Err(NetworkError::InvalidTarget);
        }
        (host, port)
    };
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(NetworkError::InvalidTarget)?;
    Ok((host.to_ascii_lowercase(), port))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credentials() -> Vec<(Role, Zeroizing<String>)> {
        vec![(Role::Rest, Zeroizing::new("Basic canary-auth".into()))]
    }
    #[test]
    fn connect_auth_and_authority_are_strict() {
        let valid = b"CONNECT discord.com:443 HTTP/1.1\r\nHost: discord.com:443\r\nProxy-Authorization: Basic canary-auth\r\n\r\n";
        assert_eq!(
            parse_connect(valid, &credentials()).unwrap(),
            ("discord.com".into(), 443, Role::Rest)
        );
        for bad in [
            "GET discord.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT discord.com:443 HTTP/1.1\r\n\r\n",
            "CONNECT discord.com:443 HTTP/1.1\r\nProxy-Authorization: Basic wrong\r\n\r\n",
            "CONNECT discord.com:443 HTTP/1.1\r\nProxy-Authorization: Basic canary-auth\r\nProxy-Authorization: Basic canary-auth\r\n\r\n",
            "CONNECT discord.com:443 HTTP/1.1\r\nProxy-Authorization: Basic canary-auth\r\nHost: evil.example:443\r\n\r\n",
            "CONNECT discord.com:443 HTTP/1.1\r\nProxy-Authorization: Basic canary-auth\r\nContent-Length: 5\r\n\r\n",
            "CONNECT discord.com:443 HTTP/1.1\r\nProxy-Authorization: Basic canary-auth\r\nTransfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(parse_connect(bad.as_bytes(), &credentials()).is_err());
        }
    }
    #[test]
    fn malformed_authorities_are_not_url_parsed() {
        for authority in [
            "user@discord.com:443",
            "discord.com:443/path",
            "discord.com:0",
            "discord.com",
            "::1:443",
            "[::1]",
            "discord.com:65536",
            "evil%2eexample:443",
        ] {
            assert!(parse_authority(authority).is_err(), "{authority}");
        }
        assert_eq!(
            parse_authority("[2606:4700:4700::1111]:443").unwrap(),
            ("2606:4700:4700::1111".into(), 443)
        );
    }
    #[tokio::test]
    async fn real_loopback_bridge_rejects_unauthenticated_and_private_origins() {
        let context = NetworkContext::new(crate::settings::Mode::Auto);
        let bridge = Bridge::start(context).unwrap();
        let mut stream = TcpStream::connect(bridge.address).await.unwrap();
        stream
            .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 407"));
        let raw = format!("fastcord-rest:{}", bridge.secret.as_str());
        let auth = base64::engine::general_purpose::STANDARD.encode(raw);
        let mut stream = TcpStream::connect(bridge.address).await.unwrap();
        stream
            .write_all(
                format!(
                    "CONNECT 127.0.0.1:1 HTTP/1.1\r\nProxy-Authorization: Basic {auth}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        response.clear();
        stream.read_to_end(&mut response).await.unwrap();
        assert!(response.starts_with(b"HTTP/1.1 403"));
    }
    #[tokio::test]
    async fn bounded_header_reader_rejects_oversize_and_pipelining() {
        for payload in [
            vec![b'a'; HEADER_LIMIT + 1],
            b"CONNECT discord.com:443 HTTP/1.1\r\n\r\nextra".to_vec(),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let mut sender = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (mut receiver, _) = listener.accept().await.unwrap();
            sender.write_all(&payload).await.unwrap();
            assert!(read_headers(&mut receiver).await.is_err());
        }
    }

    #[tokio::test]
    async fn opaque_forwarding_preserves_tls_h2_alpn_and_backpressure() {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
        use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, ServerConfig, crypto};
        let certificate =
            CertificateDer::from(include_bytes!("tls/fixtures/test-cert.der").to_vec());
        let key = include_bytes!("tls/fixtures/test-key.der");
        for strategy in Strategy::ORDER {
            let mut roots = RootCertStore::empty();
            roots.add(certificate.clone()).unwrap();
            let mut config =
                ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .with_root_certificates(roots)
                    .with_no_client_auth();
            config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            let mut server_config =
                ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                    .with_safe_default_protocol_versions()
                    .unwrap()
                    .with_no_client_auth()
                    .with_single_cert(
                        vec![certificate.clone()],
                        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.to_vec())),
                    )
                    .unwrap();
            server_config.alpn_protocols = vec![b"h2".to_vec()];
            let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
            let client = TcpStream::connect(relay.local_addr().unwrap())
                .await
                .unwrap();
            let (bridge_in, _) = relay.accept().await.unwrap();
            let bridge_out = TcpStream::connect(origin.local_addr().unwrap())
                .await
                .unwrap();
            let (server, _) = origin.accept().await.unwrap();
            client.set_nodelay(true).unwrap();
            let budget = Arc::new(Semaphore::new(4));
            let forwarded_budget = budget.clone();
            let forwarding = tokio::spawn(async move {
                forward(bridge_in, bridge_out, strategy, forwarded_budget).await
            });
            let origin = tokio::spawn(async move {
                let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(server_config))
                    .accept(server)
                    .await
                    .unwrap();
                assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                let mut preface = [0; 24];
                stream.read_exact(&mut preface).await.unwrap();
                assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
                let mut payload = vec![0; 128 * 1024];
                stream.read_exact(&mut payload).await.unwrap();
                assert!(payload.iter().all(|byte| *byte == 0x2a));
                stream.write_all(&payload).await.unwrap();
                stream.shutdown().await.unwrap();
            });
            let check = async {
                let mut stream = tokio_rustls::TlsConnector::from(Arc::new(config))
                    .connect(ServerName::try_from("localhost").unwrap(), client)
                    .await
                    .unwrap();
                assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                stream
                    .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
                    .await
                    .unwrap();
                stream.write_all(&vec![0x2a; 128 * 1024]).await.unwrap();
                stream.flush().await.unwrap();
                let mut reply = vec![0; 128 * 1024];
                stream.read_exact(&mut reply).await.unwrap();
                assert!(reply.iter().all(|byte| *byte == 0x2a));
                stream.shutdown().await.unwrap();
                drop(stream);
                origin.await.unwrap();
                forwarding.await.unwrap().unwrap();
                assert_eq!(budget.available_permits(), 4);
            };
            tokio::time::timeout(Duration::from_secs(10), check)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn reqwest_http2_request_and_response_survive_the_opaque_bridge() {
        use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
        use tokio_rustls::rustls::{ClientConfig, RootCertStore, ServerConfig, crypto};
        let certificate =
            CertificateDer::from(include_bytes!("tls/fixtures/test-cert.der").to_vec());
        let mut roots = RootCertStore::empty();
        roots.add(certificate.clone()).unwrap();
        let mut config =
            ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth();
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let mut server_config =
            ServerConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![certificate],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                        include_bytes!("tls/fixtures/test-key.der").to_vec(),
                    )),
                )
                .unwrap();
        server_config.alpn_protocols = vec![b"h2".to_vec()];
        let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let relay = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let relay_address = relay.local_addr().unwrap();
        let fixture = tokio::spawn(async move {
            let (bridge_in, _) = relay.accept().await.unwrap();
            let bridge_out = TcpStream::connect(origin.local_addr().unwrap())
                .await
                .unwrap();
            let (server, _) = origin.accept().await.unwrap();
            let forwarding = tokio::spawn(forward(
                bridge_in,
                bridge_out,
                Strategy::Combined,
                Arc::new(Semaphore::new(4)),
            ));
            let mut stream = tokio_rustls::TlsAcceptor::from(Arc::new(server_config))
                .accept(server)
                .await
                .unwrap();
            assert_eq!(stream.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
            let mut preface = [0; 24];
            stream.read_exact(&mut preface).await.unwrap();
            assert_eq!(&preface, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
            // Empty SETTINGS. Parse bounded frames only until the GET HEADERS;
            // the request's HPACK bytes remain opaque to this transport test.
            stream
                .write_all(&[0, 0, 0, 4, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            stream.flush().await.unwrap();
            loop {
                let mut header = [0; 9];
                stream.read_exact(&mut header).await.unwrap();
                let len = (usize::from(header[0]) << 16)
                    | (usize::from(header[1]) << 8)
                    | usize::from(header[2]);
                assert!(len <= 16 * 1024);
                let mut payload = vec![0; len];
                stream.read_exact(&mut payload).await.unwrap();
                if header[3] == 1 {
                    assert_eq!(&header[5..], &[0, 0, 0, 1]);
                    break;
                }
                if header[3] == 4 && header[4] & 1 == 0 {
                    stream
                        .write_all(&[0, 0, 0, 4, 1, 0, 0, 0, 0])
                        .await
                        .unwrap();
                    stream.flush().await.unwrap();
                }
            }
            // HPACK static table index8 = :status200, followed by END_STREAM data.
            stream
                .write_all(&[
                    0, 0, 1, 1, 4, 0, 0, 0, 1, 0x88, 0, 0, 4, 0, 1, 0, 0, 0, 1, b'o', b'k', b'a',
                    b'y',
                ])
                .await
                .unwrap();
            stream.flush().await.unwrap();
            stream.shutdown().await.unwrap();
            drop(stream);
            let _ = forwarding.await.unwrap();
        });
        let client = reqwest::Client::builder()
            .no_proxy()
            .use_preconfigured_tls(config)
            .resolve("localhost", relay_address)
            .build()
            .unwrap();
        let check = async {
            let response = client
                .get(format!("https://localhost:{}/", relay_address.port()))
                .send()
                .await
                .unwrap();
            assert_eq!(response.version(), reqwest::Version::HTTP_2);
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.bytes().await.unwrap().as_ref(), b"okay");
            drop(client);
            fixture.await.unwrap();
        };
        tokio::time::timeout(Duration::from_secs(10), check)
            .await
            .unwrap();
    }
}
