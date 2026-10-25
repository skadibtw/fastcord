//! Explicit user-selected TCP proxies. Destination names are sent to the proxy,
//! never resolved locally. Origin TLS remains end-to-end; SOCKS5 itself adds no
//! encryption. Media UDP is a separate capability.

use std::fmt;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpStream, lookup_host};
use tokio::time::timeout;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, pki_types::ServerName};
use zeroize::Zeroizing;

use crate::NetworkError;
use crate::broker::{BoxedIo, tls_config, valid_host};
use crate::selector::TlsProfile;

pub(crate) const DNS_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const TCP_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const TLS_TIMEOUT: Duration = Duration::from_secs(4);
pub(crate) const HEADER_TIMEOUT: Duration = Duration::from_secs(3);
pub(crate) const TOTAL_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_HEADERS: usize = 8 * 1024;
pub const MAX_CREDENTIAL_BYTES: usize = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProxyKind {
    Http,
    Https,
    Socks5,
}

impl fmt::Display for ProxyKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Http => "HTTP CONNECT",
            Self::Https => "HTTPS CONNECT",
            Self::Socks5 => "SOCKS5",
        })
    }
}

/// Only non-secret metadata may be serialized into network-settings.json.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
}

impl fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("kind", &self.kind)
            .field("host", &"[redacted]")
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl ProxyConfig {
    pub fn validate(&self) -> Result<(), NetworkError> {
        let host = self.host.trim_start_matches('[').trim_end_matches(']');
        let brackets_valid = self.host == host
            || self
                .host
                .strip_prefix('[')
                .and_then(|value| value.strip_suffix(']'))
                == Some(host);
        if self.port == 0
            || self.host.len() > 255
            || !(valid_host(host) || host.parse::<IpAddr>().is_ok())
            || !brackets_valid
            || (self.host != host && !matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))))
            || self.username.as_ref().is_some_and(|name| {
                name.is_empty()
                    || name.len() > MAX_CREDENTIAL_BYTES
                    || name.contains(':')
                    || name.chars().any(char::is_control)
            })
        {
            return Err(NetworkError::ProxyConfiguration);
        }
        if let Ok(ip) = host.parse::<IpAddr>()
            && (ip.is_unspecified()
                || ip.is_multicast()
                || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()))
        {
            return Err(NetworkError::ProxyConfiguration);
        }
        Ok(())
    }

    /// Binds a native-store credential to these exact settings. Length prefixes
    /// prevent ambiguity; account identifiers and passwords are never inputs.
    pub fn credential_key(&self) -> String {
        let mut hash = Sha256::new();
        hash.update([match self.kind {
            ProxyKind::Http => 0,
            ProxyKind::Https => 1,
            ProxyKind::Socks5 => 2,
        }]);
        let host = self
            .host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        hash.update((host.len() as u64).to_be_bytes());
        hash.update(host.as_bytes());
        hash.update(self.port.to_be_bytes());
        hash.update([u8::from(self.username.is_some())]);
        if let Some(username) = &self.username {
            hash.update((username.len() as u64).to_be_bytes());
            hash.update(username.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }
}

pub(crate) struct ProxyCredentials {
    pub(crate) config: ProxyConfig,
    password: Zeroizing<String>,
}

impl fmt::Debug for ProxyCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyCredentials")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ProxyCredentials {
    pub(crate) fn new(
        config: ProxyConfig,
        password: Zeroizing<String>,
    ) -> Result<Self, NetworkError> {
        config.validate()?;
        if password.len() > MAX_CREDENTIAL_BYTES
            || (config.username.is_none() && !password.is_empty())
            || (config.kind == ProxyKind::Socks5
                && config.username.is_some()
                && password.is_empty())
        {
            return Err(NetworkError::ProxyConfiguration);
        }
        Ok(Self { config, password })
    }
}

pub(crate) async fn dial(
    credentials: &ProxyCredentials,
    host: &str,
    port: u16,
) -> Result<BoxedIo, NetworkError> {
    dial_with_tls(
        credentials,
        host,
        port,
        tls_config(TlsProfile::WebSocket, false)?,
    )
    .await
}

pub(crate) async fn dial_with_tls(
    credentials: &ProxyCredentials,
    host: &str,
    port: u16,
    proxy_tls: Arc<ClientConfig>,
) -> Result<BoxedIo, NetworkError> {
    if port == 0 || !(valid_host(host) || host.parse::<IpAddr>().is_ok()) {
        return Err(NetworkError::InvalidTarget);
    }
    timeout(TOTAL_TIMEOUT, async {
        let tcp = connect_proxy(&credentials.config).await?;
        let mut io: BoxedIo = if credentials.config.kind == ProxyKind::Https {
            let proxy_host = credentials
                .config
                .host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned();
            let name =
                ServerName::try_from(proxy_host).map_err(|_| NetworkError::ProxyConfiguration)?;
            let tls = timeout(
                TLS_TIMEOUT,
                TlsConnector::from(proxy_tls).connect(name, tcp),
            )
            .await
            .map_err(|_| NetworkError::ProxyTimeout)?
            .map_err(|error| {
                if crate::broker::tls_error(&error) == NetworkError::CertificateRejected {
                    NetworkError::ProxyCertificateRejected
                } else {
                    NetworkError::ProxyProtocol
                }
            })?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };
        timeout(HEADER_TIMEOUT, async {
            if credentials.config.kind == ProxyKind::Socks5 {
                socks_connect(&mut io, host, port, credentials).await?;
                Ok(io)
            } else {
                http_connect(io, host, port, credentials).await
            }
        })
        .await
        .map_err(|_| NetworkError::ProxyTimeout)?
    })
    .await
    .map_err(|_| NetworkError::ProxyTimeout)?
}

pub(crate) async fn connect_proxy(config: &ProxyConfig) -> Result<TcpStream, NetworkError> {
    let host = config.host.trim_start_matches('[').trim_end_matches(']');
    let addresses = timeout(DNS_TIMEOUT, lookup_host((host, config.port)))
        .await
        .map_err(|_| NetworkError::ProxyTimeout)?
        .map_err(|_| NetworkError::ProxyDns)?;
    let mut bounded = Vec::with_capacity(16);
    for address in addresses {
        if bounded.len() == 16 {
            break;
        }
        if !address.ip().is_multicast() && !address.ip().is_unspecified() {
            bounded.push(address);
        }
    }
    if bounded.is_empty() {
        return Err(NetworkError::ProxyDns);
    }
    crate::dial::connect(&bounded, TCP_TIMEOUT, &mut None)
        .await
        .map(|(stream, _)| stream)
        .map_err(|error| match error {
            NetworkError::TcpTimeout => NetworkError::ProxyTimeout,
            _ => NetworkError::ProxyRefused,
        })
}

fn authority(host: &str, port: u16) -> String {
    if matches!(host.parse::<IpAddr>(), Ok(IpAddr::V6(_))) {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

async fn http_connect(
    mut io: BoxedIo,
    host: &str,
    port: u16,
    credentials: &ProxyCredentials,
) -> Result<BoxedIo, NetworkError> {
    let authority = authority(host, port);
    let mut request = Zeroizing::new(format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n"
    ));
    if let Some(username) = &credentials.config.username {
        let raw = Zeroizing::new(format!("{username}:{}", credentials.password.as_str()));
        let encoded =
            Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()));
        request.push_str("Proxy-Authorization: Basic ");
        request.push_str(encoded.as_str());
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    io.write_all(request.as_bytes())
        .await
        .map_err(|_| NetworkError::ProxyRefused)?;
    io.flush().await.map_err(|_| NetworkError::ProxyRefused)?;
    drop(request);
    let mut response = Zeroizing::new(Vec::with_capacity(MAX_HEADERS));
    let mut chunk = [0; 512];
    let end = loop {
        let room = MAX_HEADERS.saturating_sub(response.len());
        if room == 0 {
            return Err(NetworkError::ProxyProtocol);
        }
        let n = io
            .read(&mut chunk[..room.min(512)])
            .await
            .map_err(|_| NetworkError::ProxyRefused)?;
        if n == 0 {
            return Err(NetworkError::ProxyProtocol);
        }
        response.extend_from_slice(&chunk[..n]);
        if let Some(end) = response.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end + 4;
        }
    };
    validate_connect_response(&response[..end])?;
    if end == response.len() {
        Ok(io)
    } else {
        let prefix = response[end..].to_vec();
        Ok(Box::new(PrefixedIo {
            io,
            prefix,
            cursor: 0,
        }))
    }
}

fn validate_connect_response(headers: &[u8]) -> Result<(), NetworkError> {
    let text = std::str::from_utf8(headers).map_err(|_| NetworkError::ProxyProtocol)?;
    let mut lines = text.split("\r\n");
    let first = lines.next().ok_or(NetworkError::ProxyProtocol)?;
    let mut status = first.splitn(3, ' ');
    if !matches!(status.next(), Some("HTTP/1.0" | "HTTP/1.1")) {
        return Err(NetworkError::ProxyProtocol);
    }
    let status = status
        .next()
        .filter(|status| status.len() == 3)
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or(NetworkError::ProxyProtocol)?;
    if status == 407 {
        return Err(NetworkError::ProxyAuthentication);
    }
    if !(200..=299).contains(&status) {
        return Err(NetworkError::ProxyRefused);
    }
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(NetworkError::ProxyProtocol);
        }
        let (name, value) = line.split_once(':').ok_or(NetworkError::ProxyProtocol)?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value.chars().any(|ch| ch.is_control() && ch != '\t')
            || name.eq_ignore_ascii_case("transfer-encoding")
            || (name.eq_ignore_ascii_case("content-length") && value.trim() != "0")
        {
            return Err(NetworkError::ProxyProtocol);
        }
    }
    Ok(())
}

async fn socks_connect(
    io: &mut BoxedIo,
    host: &str,
    port: u16,
    credentials: &ProxyCredentials,
) -> Result<(), NetworkError> {
    socks_auth(io, credentials).await?;
    socks_request(io, 1, host, port).await?;
    Ok(())
}

pub(crate) async fn socks_auth<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    credentials: &ProxyCredentials,
) -> Result<(), NetworkError> {
    let method = if credentials.config.username.is_some() {
        2
    } else {
        0
    };
    io.write_all(&[5, 1, method])
        .await
        .map_err(|_| NetworkError::ProxyRefused)?;
    io.flush().await.map_err(|_| NetworkError::ProxyRefused)?;
    let mut selected = [0; 2];
    io.read_exact(&mut selected)
        .await
        .map_err(|_| NetworkError::ProxyProtocol)?;
    if selected[0] != 5 {
        return Err(NetworkError::ProxyProtocol);
    }
    if selected[1] != method {
        return Err(NetworkError::ProxyAuthentication);
    }
    if let Some(username) = &credentials.config.username {
        let mut auth = Zeroizing::new(Vec::with_capacity(
            3 + username.len() + credentials.password.len(),
        ));
        auth.extend_from_slice(&[1, username.len() as u8]);
        auth.extend_from_slice(username.as_bytes());
        auth.push(credentials.password.len() as u8);
        auth.extend_from_slice(credentials.password.as_bytes());
        io.write_all(&auth)
            .await
            .map_err(|_| NetworkError::ProxyRefused)?;
        io.flush().await.map_err(|_| NetworkError::ProxyRefused)?;
        drop(auth);
        io.read_exact(&mut selected)
            .await
            .map_err(|_| NetworkError::ProxyProtocol)?;
        if selected[0] != 1 {
            return Err(NetworkError::ProxyProtocol);
        }
        if selected[1] != 0 {
            return Err(NetworkError::ProxyAuthentication);
        }
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) enum SocksHost {
    Ip(IpAddr),
    Domain(String),
}
#[derive(Debug)]
pub(crate) struct SocksBound {
    pub(crate) host: SocksHost,
    pub(crate) port: u16,
}

pub(crate) async fn socks_request<S: AsyncRead + AsyncWrite + Unpin>(
    io: &mut S,
    command: u8,
    host: &str,
    port: u16,
) -> Result<SocksBound, NetworkError> {
    if !matches!(command, 1 | 3) {
        return Err(NetworkError::ProxyProtocol);
    }
    let mut request = Vec::with_capacity(262);
    request.extend_from_slice(&[5, command, 0]);
    encode_address(&mut request, host)?;
    request.extend_from_slice(&port.to_be_bytes());
    io.write_all(&request)
        .await
        .map_err(|_| NetworkError::ProxyRefused)?;
    io.flush().await.map_err(|_| NetworkError::ProxyRefused)?;
    let mut header = [0; 4];
    io.read_exact(&mut header)
        .await
        .map_err(|_| NetworkError::ProxyProtocol)?;
    if header[0] != 5 || header[2] != 0 {
        return Err(NetworkError::ProxyProtocol);
    }
    if header[1] != 0 {
        return Err(NetworkError::ProxyRefused);
    }
    let length = match header[3] {
        1 => 4,
        4 => 16,
        3 => {
            let n = io
                .read_u8()
                .await
                .map_err(|_| NetworkError::ProxyProtocol)?;
            if n == 0 {
                return Err(NetworkError::ProxyProtocol);
            }
            usize::from(n)
        }
        _ => return Err(NetworkError::ProxyProtocol),
    };
    let mut address = [0; 257];
    io.read_exact(&mut address[..length + 2])
        .await
        .map_err(|_| NetworkError::ProxyProtocol)?;
    let port = u16::from_be_bytes([address[length], address[length + 1]]);
    let host = match header[3] {
        1 => SocksHost::Ip(IpAddr::V4(std::net::Ipv4Addr::new(
            address[0], address[1], address[2], address[3],
        ))),
        4 => SocksHost::Ip(IpAddr::V6(std::net::Ipv6Addr::from(
            <[u8; 16]>::try_from(&address[..16]).map_err(|_| NetworkError::ProxyProtocol)?,
        ))),
        3 => {
            let name =
                std::str::from_utf8(&address[..length]).map_err(|_| NetworkError::ProxyProtocol)?;
            if !valid_host(name) {
                return Err(NetworkError::ProxyProtocol);
            }
            SocksHost::Domain(name.to_owned())
        }
        _ => return Err(NetworkError::ProxyProtocol),
    };
    Ok(SocksBound { host, port })
}

fn encode_address(buffer: &mut Vec<u8>, host: &str) -> Result<(), NetworkError> {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            buffer.push(1);
            buffer.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            buffer.push(4);
            buffer.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            if !valid_host(host) {
                return Err(NetworkError::InvalidTarget);
            }
            buffer.extend_from_slice(&[3, host.len() as u8]);
            buffer.extend_from_slice(host.as_bytes());
        }
    }
    Ok(())
}

struct PrefixedIo {
    io: BoxedIo,
    prefix: Vec<u8>,
    cursor: usize,
}
impl AsyncRead for PrefixedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.cursor < self.prefix.len() {
            let n = buffer.remaining().min(self.prefix.len() - self.cursor);
            buffer.put_slice(&self.prefix[self.cursor..self.cursor + n]);
            self.cursor += n;
            if self.cursor == self.prefix.len() {
                self.prefix = Vec::new();
            }
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.io).poll_read(cx, buffer)
    }
}
impl AsyncWrite for PrefixedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(cx, buffer)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

#[cfg(test)]
#[path = "proxy/tests.rs"]
mod tests;

#[cfg(test)]
#[path = "proxy/integration.rs"]
mod integration;
