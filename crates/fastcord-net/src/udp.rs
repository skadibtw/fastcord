//! SOCKS5 UDP ASSOCIATE is independent of the HTTP/TCP proxy transport. The
//! control connection remains open for the association's entire lifetime.
//! This does not provide UDP-over-TCP or encryption of the proxy transport.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use tokio::net::{UdpSocket, lookup_host};
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::NetworkError;
use crate::proxy::{self, ProxyCredentials, ProxyKind, SocksBound, SocksHost};

pub const MAX_ASSOCIATIONS: usize = 8;
/// A 1500-byte IP MTU, without claiming that an actual route supports it.
pub const OUTER_IPV4_PAYLOAD: usize = 1472;
pub const OUTER_IPV6_PAYLOAD: usize = 1452;
// Receive a complete maximum UDP datagram, then apply the much smaller relay
// MTU cap. A small receive buffer can turn an oversized datagram into an I/O
// error instead of a length on some OSes. This allocation is reused, not made
// or cleared on each packet; eight association permits cap it at 512 KiB.
const RECEIVE_CAPACITY: usize = 65_536;

pub enum DatagramSocket {
    Direct(UdpSocket),
    Socks5(Association),
}

impl fmt::Debug for DatagramSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatagramSocket")
            .field("relay", &self.is_relay())
            .finish_non_exhaustive()
    }
}

impl DatagramSocket {
    pub fn is_relay(&self) -> bool {
        matches!(self, Self::Socks5(_))
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Direct(socket) => socket.local_addr(),
            Self::Socks5(association) => association.socket.local_addr(),
        }
    }

    /// Maximum inner UDP payload using the stated 1500-byte outer IP MTU.
    /// SOCKS5's address header is included in the relay's accounting.
    pub fn max_payload(&self) -> usize {
        match self {
            Self::Direct(socket) => {
                if socket.local_addr().is_ok_and(|address| address.is_ipv6()) {
                    OUTER_IPV6_PAYLOAD
                } else {
                    OUTER_IPV4_PAYLOAD
                }
            }
            Self::Socks5(association) => association.max_payload,
        }
    }

    pub async fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<usize> {
        match self {
            // Preserve the pre-existing direct socket semantics. The direct
            // max_payload value is a packetization recommendation; the relay
            // cap below is enforced before its encapsulation is allocated.
            Self::Direct(socket) => socket.send_to(datagram, to).await,
            Self::Socks5(association) => association.send_to(datagram, to).await,
        }
    }

    /// Invalid relay/source/header datagrams are discarded. Only the exact
    /// media endpoint bound to this association can reach the voice driver.
    pub async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Direct(socket) => socket.recv_from(buffer).await,
            Self::Socks5(association) => association.recv_from(buffer).await,
        }
    }

    pub(crate) async fn associate(
        credentials: Arc<ProxyCredentials>,
        server: SocketAddr,
        slots: Arc<Semaphore>,
    ) -> io::Result<Self> {
        if credentials.config.kind != ProxyKind::Socks5 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                NetworkError::Unsupported,
            ));
        }
        if !crate::broker::public_ip(server.ip()) || server.port() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                NetworkError::InvalidTarget,
            ));
        }
        let permit = slots
            .try_acquire_owned()
            .map_err(|_| io::Error::other(NetworkError::BudgetExhausted))?;
        let association = timeout(proxy::TOTAL_TIMEOUT, async move {
            let mut control = proxy::connect_proxy(&credentials.config).await?;
            timeout(
                proxy::HEADER_TIMEOUT,
                proxy::socks_auth(&mut control, &credentials),
            )
            .await
            .map_err(|_| NetworkError::ProxyTimeout)??;
            let control_peer = control
                .peer_addr()
                .map_err(|_| NetworkError::ProxyRefused)?;
            let local_ip = control
                .local_addr()
                .map_err(|_| NetworkError::ProxyRefused)?
                .ip();
            // Bind a real UDP socket before association, but do not advertise
            // this local endpoint as the proxy-visible source through NAT.
            let socket = UdpSocket::bind(SocketAddr::new(local_ip, 0))
                .await
                .map_err(|_| NetworkError::ProxyRefused)?;
            // RFC1928 section 6 requires all zeros when the client does not
            // know its source endpoint as the relay will see it. The relay
            // learns the actual UDP source; the TCP source port is irrelevant.
            let unknown_source: IpAddr = if local_ip.is_ipv4() {
                Ipv4Addr::UNSPECIFIED.into()
            } else {
                Ipv6Addr::UNSPECIFIED.into()
            };
            let bound = timeout(
                proxy::HEADER_TIMEOUT,
                proxy::socks_request(&mut control, 3, &unknown_source.to_string(), 0),
            )
            .await
            .map_err(|_| NetworkError::ProxyTimeout)??;
            let relay = relay_address(bound, control_peer).await?;
            let outer_limit = if relay.is_ipv4() {
                OUTER_IPV4_PAYLOAD
            } else {
                OUTER_IPV6_PAYLOAD
            };
            let max_payload = outer_limit - header_size(server);
            let (closed, receiver) = watch::channel(false);
            let task = tokio::spawn(async move {
                // RFC1928 defines no further data on this control connection.
                // EOF, I/O error or unexpected data all end the association.
                let mut byte = [0];
                let _ = control.read(&mut byte).await;
                let _ = closed.send(true);
            });
            Ok::<_, NetworkError>(Association {
                socket,
                receive: Mutex::new(vec![0; RECEIVE_CAPACITY].into_boxed_slice()),
                relay,
                server,
                outer_limit,
                max_payload,
                control: Control {
                    task,
                    closed: receiver,
                },
                _permit: permit,
            })
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, NetworkError::ProxyTimeout))?
        .map_err(io::Error::other)?;
        Ok(Self::Socks5(association))
    }
}

pub struct Association {
    socket: UdpSocket,
    receive: Mutex<Box<[u8]>>,
    relay: SocketAddr,
    server: SocketAddr,
    outer_limit: usize,
    max_payload: usize,
    control: Control,
    _permit: OwnedSemaphorePermit,
}

struct Control {
    task: JoinHandle<()>,
    closed: watch::Receiver<bool>,
}
impl Drop for Control {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn closed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        NetworkError::ProxyUdpClosed,
    )
}

async fn wait_closed(mut closed: watch::Receiver<bool>) {
    loop {
        if *closed.borrow() {
            return;
        }
        if closed.changed().await.is_err() {
            return;
        }
    }
}

impl Association {
    async fn send_to(&self, datagram: &[u8], to: SocketAddr) -> io::Result<usize> {
        if *self.control.closed.borrow() {
            return Err(closed_error());
        }
        if to != self.server {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                NetworkError::InvalidTarget,
            ));
        }
        if datagram.len() > self.max_payload {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Media packet exceeds the SOCKS5 UDP relay datagram limit.",
            ));
        }
        let mut encoded = [0; OUTER_IPV4_PAYLOAD];
        let length = encode_datagram(to, datagram, &mut encoded)?;
        let sent = tokio::select! {
            biased;
            _ = wait_closed(self.control.closed.clone()) => return Err(closed_error()),
            result = self.socket.send_to(&encoded[..length], self.relay) => result,
        }
        .map_err(|_| io::Error::other(NetworkError::ProxyRefused))?;
        if sent != length {
            return Err(io::ErrorKind::WriteZero.into());
        }
        Ok(datagram.len())
    }

    async fn recv_from(&self, buffer: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        let mut encoded = tokio::select! {
            biased;
            _ = wait_closed(self.control.closed.clone()) => return Err(closed_error()),
            received = self.receive.lock() => received,
        };
        let mut rejected = 0u8;
        loop {
            if *self.control.closed.borrow() {
                return Err(closed_error());
            }
            let (length, source) = tokio::select! {
                biased;
                _ = wait_closed(self.control.closed.clone()) => return Err(closed_error()),
                result = self.socket.recv_from(&mut encoded) => result,
            }
            .map_err(|_| io::Error::other(NetworkError::ProxyRefused))?;
            if source == self.relay
                && length <= self.outer_limit
                && let Some(body) = parse_datagram(&encoded[..length], self.server)
            {
                let copied = body.len().min(buffer.len());
                buffer[..copied].copy_from_slice(&body[..copied]);
                return Ok((copied, self.server));
            }
            rejected += 1;
            if rejected == 16 {
                rejected = 0;
                tokio::task::yield_now().await;
            }
        }
    }
}

async fn relay_address(
    bound: SocksBound,
    control_peer: SocketAddr,
) -> Result<SocketAddr, NetworkError> {
    if bound.port == 0 {
        return Err(NetworkError::ProxyProtocol);
    }
    let address = match bound.host {
        SocksHost::Ip(ip) => {
            if ip.is_ipv4() != control_peer.is_ipv4() {
                return Err(NetworkError::ProxyProtocol);
            }
            SocketAddr::new(
                if ip.is_unspecified() {
                    control_peer.ip()
                } else {
                    ip
                },
                bound.port,
            )
        }
        SocksHost::Domain(host) => {
            let addresses = timeout(proxy::DNS_TIMEOUT, lookup_host((host.as_str(), bound.port)))
                .await
                .map_err(|_| NetworkError::ProxyTimeout)?
                .map_err(|_| NetworkError::ProxyDns)?;
            let mut selected = None;
            for address in addresses.take(16) {
                if valid_relay(address, control_peer) {
                    if address.ip() == control_peer.ip() {
                        selected = Some(address);
                        break;
                    }
                    if selected.is_none() {
                        selected = Some(address);
                    }
                }
            }
            selected.ok_or(NetworkError::ProxyProtocol)?
        }
    };
    if !valid_relay(address, control_peer) {
        return Err(NetworkError::ProxyProtocol);
    }
    Ok(address)
}

fn valid_relay(address: SocketAddr, control_peer: SocketAddr) -> bool {
    let ip = address.ip();
    address.port() != 0
        && address.is_ipv4() == control_peer.is_ipv4()
        && !ip.is_unspecified()
        && !ip.is_multicast()
        && (!ip.is_loopback() || control_peer.ip().is_loopback())
        && (!crate::broker::public_ip(control_peer.ip()) || crate::broker::public_ip(ip))
        && match ip {
            IpAddr::V4(ip) => !ip.is_broadcast() && !ip.is_link_local(),
            IpAddr::V6(ip) => !ip.is_unicast_link_local(),
        }
}

fn header_size(server: SocketAddr) -> usize {
    if server.is_ipv4() { 10 } else { 22 }
}

fn encode_datagram(server: SocketAddr, body: &[u8], output: &mut [u8]) -> io::Result<usize> {
    let header = header_size(server);
    let length = header
        .checked_add(body.len())
        .ok_or(io::ErrorKind::InvalidInput)?;
    if server.port() == 0 || length > output.len() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    output[..3].fill(0);
    match server.ip() {
        IpAddr::V4(ip) => {
            output[3] = 1;
            output[4..8].copy_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            output[3] = 4;
            output[4..20].copy_from_slice(&ip.octets());
        }
    }
    output[header - 2..header].copy_from_slice(&server.port().to_be_bytes());
    output[header..length].copy_from_slice(body);
    Ok(length)
}

fn parse_datagram(packet: &[u8], expected: SocketAddr) -> Option<&[u8]> {
    if packet.get(..3)? != [0, 0, 0] {
        return None;
    }
    let (ip, header) = match *packet.get(3)? {
        1 => (
            IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(packet.get(4..8)?).ok()?)),
            10,
        ),
        4 => (
            IpAddr::V6(Ipv6Addr::from(
                <[u8; 16]>::try_from(packet.get(4..20)?).ok()?,
            )),
            22,
        ),
        // Media endpoints are concrete READY addresses. A domain source is
        // neither locally resolved nor treated as equivalent to that endpoint.
        _ => return None,
    };
    let port = u16::from_be_bytes(<[u8; 2]>::try_from(packet.get(header - 2..header)?).ok()?);
    (SocketAddr::new(ip, port) == expected).then(|| &packet[header..])
}

#[cfg(test)]
#[path = "udp/tests.rs"]
mod tests;
