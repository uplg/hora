//! TCP connects that survive one dead address: a name with an IPv6 address
//! that blackholes and a working IPv4 one must not eat the whole timeout on
//! the first before ever trying the second.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;
use tokio::net::TcpStream;

/// How long an attempt runs alone before the next address joins the race -
/// RFC 8305's recommended "Connection Attempt Delay".
const ATTEMPT_DELAY: Duration = Duration::from_millis(250);

/// Connect to `host:port` within `timeout`, happy-eyeballs style (RFC 8305,
/// simplified): the resolved addresses are interleaved by family, the first
/// is tried, and every [`ATTEMPT_DELAY`] without an answer (or at once, after
/// a refusal) the next one starts alongside. The first connection wins; the
/// others are dropped.
///
/// # Errors
///
/// The resolution error, a timeout (`ErrorKind::TimedOut`), or the last
/// connect error when every address failed.
pub(crate) async fn connect(host: &str, port: u16, timeout: Duration) -> io::Result<TcpStream> {
    let race = async {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
        race(interleave(addrs)).await
    };
    tokio::time::timeout(timeout, race)
        .await
        .map_err(|_elapsed| io::Error::new(io::ErrorKind::TimedOut, "connection timed out"))?
}

/// Alternate address families, keeping the resolver's order within each and
/// starting with the family it put first.
fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let Some(first) = addrs.first().copied() else {
        return addrs;
    };
    let (mut lead, mut other): (Vec<_>, Vec<_>) = addrs
        .into_iter()
        .partition(|addr| addr.is_ipv6() == first.is_ipv6());
    let mut out = Vec::with_capacity(lead.len() + other.len());
    lead.reverse();
    other.reverse();
    while !lead.is_empty() || !other.is_empty() {
        out.extend(lead.pop());
        out.extend(other.pop());
    }
    out
}

/// Race the connects to `addrs` (see [`connect`]).
pub(crate) async fn race(addrs: Vec<SocketAddr>) -> io::Result<TcpStream> {
    race_with(addrs, TcpStream::connect).await
}

/// [`race`], with the connect to one address handed in: a test decides which
/// address answers and which never does, whatever the network it runs on.
async fn race_with<T, F>(addrs: Vec<SocketAddr>, connect: impl Fn(SocketAddr) -> F) -> io::Result<T>
where
    F: Future<Output = io::Result<T>>,
{
    let mut queue = addrs.into_iter();
    let mut attempts = FuturesUnordered::new();
    let mut last_error = None;
    loop {
        if attempts.is_empty() {
            match queue.next() {
                Some(addr) => attempts.push(connect(addr)),
                None => {
                    return Err(last_error.unwrap_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no address for host")
                    }));
                }
            }
        }
        let more = queue.len() > 0;
        tokio::select! {
            Some(result) = attempts.next() => match result {
                Ok(stream) => return Ok(stream),
                // A refusal is an answer: no reason to wait out the delay.
                Err(err) => {
                    last_error = Some(err);
                    if let Some(addr) = queue.next() {
                        attempts.push(connect(addr));
                    }
                }
            },
            () = tokio::time::sleep(ATTEMPT_DELAY), if more => {
                if let Some(addr) = queue.next() {
                    attempts.push(connect(addr));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_alternate_from_the_resolvers_first() {
        let addr = |raw: &str| raw.parse::<SocketAddr>().unwrap();
        let interleaved = interleave(vec![
            addr("[2001:db8::1]:443"),
            addr("[2001:db8::2]:443"),
            addr("192.0.2.1:443"),
            addr("192.0.2.2:443"),
            addr("192.0.2.3:443"),
        ]);
        assert_eq!(
            interleaved,
            vec![
                addr("[2001:db8::1]:443"),
                addr("192.0.2.1:443"),
                addr("[2001:db8::2]:443"),
                addr("192.0.2.2:443"),
                addr("192.0.2.3:443"),
            ]
        );
    }

    /// A first address that never answers does not keep the probe from the
    /// working second one. The silence is played here, not left to the
    /// network: TEST-NET-1 is not routed, but a network that intercepts
    /// connections answers for it.
    #[tokio::test]
    async fn a_blackholed_address_does_not_eat_the_timeout() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good = listener.local_addr().unwrap();
        let dead: SocketAddr = format!("192.0.2.1:{}", good.port()).parse().unwrap();
        let start = std::time::Instant::now();
        let connect = |addr: SocketAddr| async move {
            if addr == dead {
                std::future::pending::<()>().await;
            }
            TcpStream::connect(addr).await
        };
        let racing = race_with(vec![dead, good], connect);
        let stream = tokio::time::timeout(Duration::from_secs(5), racing)
            .await
            .expect("well within the timeout")
            .expect("connected");
        assert_eq!(stream.peer_addr().unwrap(), good);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn every_address_refusing_returns_the_last_error() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = listener.local_addr().unwrap();
        drop(listener);
        let err = race(vec![closed, closed]).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::ConnectionRefused);
    }
}
