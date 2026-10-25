//! Two bounded attempts, with a different address family preferred for the
//! fallback. Each gets its own deadline; a stalled first address cannot consume
//! the second one's allowance.
use crate::NetworkError;
use std::net::SocketAddr;
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
