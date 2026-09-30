#[cfg(feature = "server")]
use anyhow::Context;
use anyhow::{Result, anyhow};
use async_http_proxy::{http_connect_tokio, http_connect_tokio_with_basic_auth};
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
use url::Url;

use crate::transport::AddrMaybeCached;

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

pub fn host_port_pair(s: &str) -> Result<(&str, u16)> {
    let semi = s
        .rfind(':')
        .ok_or_else(|| anyhow!("Address is missing the port: {s}"))?;
    Ok((&s[..semi], s[semi + 1..].parse()?))
}

#[cfg(feature = "client")]
/// Create a UDP socket and connect to `addr`
pub async fn udp_connect<A: ToSocketAddrs>(addr: A, prefer_ipv6: bool) -> Result<UdpSocket> {
    let (socket_addr, bind_addr);

    if prefer_ipv6 {
        let all_host_addresses: Vec<SocketAddr> = lookup_host(addr).await?.collect();

        // Try to find an IPv6 address, falling back to IPv4 when the host
        // only exposes A records.
        let found = all_host_addresses.iter().find(|x| x.is_ipv6());
        let fallback = all_host_addresses.iter().find(|x| x.is_ipv4());
        match found.or(fallback) {
            Some(addr) => {
                bind_addr = if addr.is_ipv6() { ":::0" } else { "0.0.0.0:0" };
                socket_addr = *addr;
            }
            None => return Err(anyhow!("Failed to lookup the host")),
        }
    } else {
        socket_addr = to_socket_addr(addr).await?;

        bind_addr = match socket_addr {
            SocketAddr::V4(_) => "0.0.0.0:0",
            SocketAddr::V6(_) => ":::0",
        };
    }
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

/// Create a `TcpStream` using a proxy
/// e.g. `<socks5://user:pass@127.0.0.1:1080>` `<http://127.0.0.1:8080>`
pub async fn tcp_connect_with_proxy(
    addr: &AddrMaybeCached,
    proxy: Option<&Url>,
) -> Result<TcpStream> {
    if let Some(url) = proxy {
        let addr = &addr.addr;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("Proxy URL is missing the host: {url}"))?;
        let port = url
            .port()
            .ok_or_else(|| anyhow!("Proxy URL is missing the port: {url}"))?;
        let mut s = TcpStream::connect((host, port)).await?;

        let auth = if !url.username().is_empty() || url.password().is_some() {
            Some(async_socks5::Auth {
                username: url.username().into(),
                password: url.password().unwrap_or("").into(),
            })
        } else {
            None
        };
        match url.scheme() {
            "socks5" => {
                async_socks5::connect(&mut s, host_port_pair(addr)?, auth).await?;
            }
            "http" => {
                let (host, port) = host_port_pair(addr)?;
                match auth {
                    Some(auth) => {
                        http_connect_tokio_with_basic_auth(
                            &mut s,
                            host,
                            port,
                            &auth.username,
                            &auth.password,
                        )
                        .await?;
                    }
                    None => http_connect_tokio(&mut s, host, port).await?,
                }
            }
            scheme => return Err(anyhow!("Unknown proxy scheme: {scheme}")),
        }
        Ok(s)
    } else {
        Ok(match addr.socket_addr {
            Some(s) => TcpStream::connect(s).await?,
            None => TcpStream::connect(&addr.addr).await?,
        })
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
}
