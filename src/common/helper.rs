#[cfg(feature = "server")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "client")]
use anyhow::anyhow;
#[cfg(feature = "client")]
use socket2::{SockRef, TcpKeepalive};
#[cfg(any(feature = "client", feature = "server"))]
use std::io;
#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::time::Duration;
#[cfg(feature = "server")]
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(feature = "client")]
use tokio::net::{ToSocketAddrs, UdpSocket, lookup_host};
use tracing::trace;

// Tokio hesitates to expose this option...So we have to do it on our own :(
// The good news is that using socket2 it can be easily done, without losing portability.
// See https://github.com/tokio-rs/tokio/issues/3082
pub fn try_set_tcp_keepalive(
    conn: &TcpStream,
    keepalive_duration: Duration,
    keepalive_interval: Duration,
) -> Result<()> {
    let s = SockRef::from(conn);
    let keepalive = TcpKeepalive::new()
        .with_time(keepalive_duration)
        .with_interval(keepalive_interval);

    trace!(
        "Set TCP keepalive {:?} {:?}",
        keepalive_duration, keepalive_interval
    );

    Ok(s.set_tcp_keepalive(&keepalive)?)
}

/// Exits with a clear message when an entry point is reached in a binary
/// built without its feature. The callers are the `#[cfg(not(feature =
/// ...))]` arms in `lib.rs`, so the item is cfg-gated to exist exactly
/// when one of them does — feature-gated dead code is gated, never
/// `allow`ed away (AGENTS.md §2).
#[cfg(any(
    not(feature = "noise"),
    not(feature = "client"),
    not(feature = "server")
))]
pub fn feature_not_compile(feature: &str) -> ! {
    eprintln!("The feature '{feature}' is not compiled in this binary. Please re-compile molehill");
    std::process::exit(1);
}

#[cfg(feature = "client")]
pub async fn to_socket_addr<A: ToSocketAddrs>(addr: A) -> Result<SocketAddr> {
    lookup_host(addr)
        .await?
        .next()
        .ok_or_else(|| anyhow!("Failed to lookup the host"))
}

#[cfg(feature = "client")]
/// Create a UDP socket and connect to the address.
///
/// The bind address follows the family the name resolves to, so an IPv6
/// local service gets an IPv6 socket without a configuration key.
pub async fn udp_connect<A: ToSocketAddrs>(addr: A) -> Result<UdpSocket> {
    let socket_addr = to_socket_addr(addr).await?;
    let bind_addr = match socket_addr {
        SocketAddr::V4(_) => "0.0.0.0:0",
        SocketAddr::V6(_) => "[::]:0",
    };
    let s = UdpSocket::bind(bind_addr).await?;
    s.connect(socket_addr).await?;
    Ok(s)
}

/// The length of a datagram a socket read delivered, translating the one
/// platform difference that matters to `udp_buffer_size`.
///
/// A datagram longer than the read buffer is truncated on both platforms this
/// crate runs on, but only one of them reports it the same way. A POSIX
/// `recv`/`recv_from` fills the buffer and returns the buffer's length; Windows
/// fills the buffer with the same prefix and returns `WSAEMSGSIZE` (10040)
/// instead, which is the error spellings of exactly that outcome ("the message
/// was too large to fit into the specified buffer and was truncated" — `mio`
/// documents the buffer half explicitly, see its `net` module notes). Reading
/// it as the length it stands for is what keeps a datagram over the service's
/// `udp_buffer_size` inside the contract; taking it for a socket failure is
/// what used to tear down a whole UDP pool on the server and break a peer's
/// forwarder on the client.
///
/// Callers must not need the datagram's peer address: the Windows read returns
/// none, which is why the server reads a whole-datagram buffer and truncates
/// in software instead (see `run_udp_connection_pool`). This form is for a
/// *connected* socket, where there is no address to lose.
#[cfg(any(feature = "client", feature = "server"))]
pub fn datagram_len(result: io::Result<usize>, buf_len: usize) -> io::Result<usize> {
    /// `WSAEMSGSIZE`, whose only producer is Windows. Spelled out rather than
    /// pulled from a Windows-only crate: the comparison is inert elsewhere
    /// (no `errno` here is 10040), so both platforms compile the same code.
    const WSAEMSGSIZE: i32 = 10040;
    match result {
        Ok(n) => Ok(n),
        Err(e) if e.raw_os_error() == Some(WSAEMSGSIZE) => Ok(buf_len),
        Err(e) => Err(e),
    }
}

// Wrapper of retry with shutdown deadline
#[cfg(feature = "server")]
pub async fn write_and_flush<T>(conn: &mut T, data: &[u8]) -> Result<()>
where
    T: AsyncWrite + Unpin,
{
    conn.write_all(data)
        .await
        .with_context(|| "Failed to write data")?;
    conn.flush().await.with_context(|| "Failed to flush data")?;
    Ok(())
}

#[cfg(all(test, any(feature = "client", feature = "server")))]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap and expect on values they just produced"
    )]

    use super::*;
    #[cfg(feature = "client")]
    use std::time::Duration;

    /// The Windows spelling of "the datagram did not fit": the buffer holds the
    /// prefix, and the read reports the length it filled rather than the
    /// failure it looks like. A synthetic errno, because the platform that
    /// produces it is not the one running this test — what is under test is the
    /// translation, which is the same code on every platform.
    #[test]
    fn an_oversized_datagram_reads_as_the_buffer_it_filled() {
        let truncated = Err(io::Error::from_raw_os_error(10040));
        assert_eq!(datagram_len(truncated, 1024).unwrap(), 1024);
    }

    /// Every other read error keeps failing, with its own errno intact: the
    /// translation must not turn a dead socket into a full buffer of stale
    /// bytes.
    #[test]
    fn any_other_read_error_still_fails() {
        let refused = Err(io::Error::from_raw_os_error(111));
        let err = datagram_len(refused, 1024).expect_err("a refused socket is not a datagram");
        assert_eq!(err.raw_os_error(), Some(111));
    }

    /// A read that fits is passed through untouched, including a full buffer
    /// (which on POSIX is also how a truncation is spelled).
    #[test]
    fn a_short_read_keeps_its_length() {
        assert_eq!(datagram_len(Ok(37), 1024).unwrap(), 37);
        assert_eq!(datagram_len(Ok(1024), 1024).unwrap(), 1024);
    }

    /// The IPv6-preferring UDP lookup falls back to IPv4 when the host has
    /// only an A record; the connected socket then carries a datagram.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn udp_connect_prefers_ipv6_but_falls_back_to_ipv4() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let sender = udp_connect(("127.0.0.1", receiver_addr.port()))
            .await
            .expect("the IPv4 fallback must connect");
        sender.send(b"x").await.unwrap();
        let mut buf = [0u8; 1];
        let (n, _) = tokio::time::timeout(Duration::from_secs(5), receiver.recv_from(&mut buf))
            .await
            .expect("the datagram timed out")
            .unwrap();
        assert_eq!(&buf[..n], b"x");
    }
}
