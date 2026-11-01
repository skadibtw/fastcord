//! Two bounded attempts, with a different address family preferred for the
//! fallback. Each gets its own deadline; a stalled first address cannot consume
//! the second one's allowance.
use crate::NetworkError;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::timeout;

pub(crate) fn candidates(addresses: &[SocketAddr]) -> Vec<SocketAddr> {
    let Some(first) = addresses.first().copied() else {
        return Vec::new();
    };
    let second = addresses
        .iter()
        .copied()
        .find(|a| a.is_ipv4() != first.is_ipv4())
        .or_else(|| addresses.iter().copied().find(|a| *a != first));
    std::iter::once(first).chain(second).collect()
}

pub(crate) async fn connect(
    addresses: &[SocketAddr],
    deadline: Duration,
    failed: &mut Option<SocketAddr>,
) -> Result<(TcpStream, SocketAddr), NetworkError> {
    let (stream, address) = connect_with(addresses, deadline, failed, TcpStream::connect).await?;
    stream
        .set_nodelay(true)
        .map_err(|_| NetworkError::NoRoute)?;
    Ok((stream, address))
}

/// A selected source is strict: no incompatible-family or unbound fallback.
pub(crate) fn local_endpoint(
    source: Option<IpAddr>,
    remote: SocketAddr,
) -> std::io::Result<SocketAddr> {
    let ip = source.unwrap_or_else(|| {
        if remote.is_ipv4() {
            std::net::Ipv4Addr::UNSPECIFIED.into()
        } else {
            std::net::Ipv6Addr::UNSPECIFIED.into()
        }
    });
    if ip.is_ipv4() != remote.is_ipv4() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "Source address family does not match destination",
        ));
    }
    Ok(SocketAddr::new(ip, 0))
}

pub(crate) async fn connect_one(
    address: SocketAddr,
    source: Option<IpAddr>,
) -> std::io::Result<TcpStream> {
    let Some(_) = source else {
        return TcpStream::connect(address).await;
    };
    let local = local_endpoint(source, address)?;
    let socket = if address.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    socket.bind(local)?;
    socket.connect(address).await
}

pub(crate) async fn connect_bound(
    addresses: &[SocketAddr],
    deadline: Duration,
    failed: &mut Option<SocketAddr>,
    source: Option<IpAddr>,
) -> Result<(TcpStream, SocketAddr), NetworkError> {
    if source.is_none() {
        return connect(addresses, deadline, failed).await;
    }
    let addresses: Vec<_> = addresses
        .iter()
        .copied()
        .filter(|a| source.is_none_or(|ip| ip.is_ipv4() == a.is_ipv4()))
        .collect();
    let (stream, address) = connect_with(&addresses, deadline, failed, |address| {
        connect_one(address, source)
    })
    .await?;
    stream
        .set_nodelay(true)
        .map_err(|_| NetworkError::NoRoute)?;
    Ok((stream, address))
}

async fn connect_with<F, Fut, T>(
    addresses: &[SocketAddr],
    deadline: Duration,
    failed: &mut Option<SocketAddr>,
    mut dial: F,
) -> Result<(T, SocketAddr), NetworkError>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    let mut last = NetworkError::NoRoute;
    for address in candidates(addresses) {
        *failed = Some(address);
        match timeout(deadline, dial(address)).await {
            Ok(Ok(stream)) => return Ok((stream, address)),
            Ok(Err(error)) => last = crate::broker::tcp_error(&error),
            Err(_) => last = NetworkError::TcpTimeout,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn selected_source_reaches_real_socket_and_filters_incompatible_family() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source: IpAddr = "127.0.0.1".parse().unwrap();
        let remote = listener.local_addr().unwrap();
        let addresses = ["[::1]:1".parse().unwrap(), remote];
        let (stream, chosen) =
            connect_bound(&addresses, Duration::from_secs(1), &mut None, Some(source))
                .await
                .unwrap();
        let (peer, observed) = listener.accept().await.unwrap();
        assert_eq!(chosen, remote);
        assert_eq!(stream.local_addr().unwrap().ip(), source);
        assert_eq!(observed.ip(), source);
        drop((stream, peer));
        assert!(
            connect_bound(
                &addresses[..1],
                Duration::from_secs(1),
                &mut None,
                Some(source)
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn unassigned_source_fails_without_retrying_unbound_and_udp_uses_same_family_rule() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = listener.local_addr().unwrap();
        let missing: IpAddr = "192.0.2.254".parse().unwrap();
        assert!(
            connect_bound(
                &[remote],
                Duration::from_millis(100),
                &mut None,
                Some(missing)
            )
            .await
            .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
        let source: IpAddr = "127.0.0.1".parse().unwrap();
        let udp = tokio::net::UdpSocket::bind(local_endpoint(Some(source), remote).unwrap())
            .await
            .unwrap();
        assert_eq!(udp.local_addr().unwrap().ip(), source);
        assert!(local_endpoint(Some(source), "[::1]:1234".parse().unwrap()).is_err());
    }

    fn addresses() -> [SocketAddr; 3] {
        ["[2606:4700::1]:443", "[2606:4700::2]:443", "1.1.1.1:443"].map(|s| s.parse().unwrap())
    }
    #[test]
    fn fallback_uses_the_other_family_before_a_second_address_of_the_first() {
        let [a, b, c] = addresses();
        assert_eq!(candidates(&[a, b, c]), [a, c]);
        assert_eq!(candidates(&[c, "8.8.8.8:443".parse().unwrap(), a]), [c, a]);
        assert_eq!(candidates(&[a, a, b]), [a, b]);
        assert_eq!(candidates(&[a, a]), [a]);
        assert!(candidates(&[]).is_empty());
    }
    #[tokio::test(start_paused = true)]
    async fn a_stalled_ipv6_attempt_leaves_a_full_deadline_for_ipv4() {
        let start = tokio::time::Instant::now();
        let [a, _, c] = addresses();
        let mut failed = None;
        let ((), reached) = connect_with(
            &addresses(),
            Duration::from_secs(3),
            &mut failed,
            |address| async move {
                if address.is_ipv6() {
                    std::future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            },
        )
        .await
        .unwrap();
        assert_eq!(reached, c);
        assert_ne!(reached, a);
        assert_eq!(failed, Some(c));
        assert_eq!(start.elapsed(), Duration::from_secs(5));
    }
    #[tokio::test(start_paused = true)]
    async fn two_timeouts_end_after_two_attempts_without_trying_every_dns_answer() {
        let start = tokio::time::Instant::now();
        let mut count = 0;
        let result = connect_with(&addresses(), Duration::from_secs(3), &mut None, |_| {
            count += 1;
            std::future::pending::<std::io::Result<()>>()
        })
        .await;
        assert_eq!(result, Err(NetworkError::TcpTimeout));
        assert_eq!(count, 2);
        assert_eq!(start.elapsed(), Duration::from_secs(6));
    }
}
