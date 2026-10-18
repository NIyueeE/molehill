#[cfg(feature = "server")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "client")]
use anyhow::anyhow;
#[cfg(feature = "client")]
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
#[cfg(feature = "client")]
use url::Url;

#[cfg(feature = "client")]
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

#[cfg(feature = "client")]
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
#[cfg(feature = "client")]
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

    #[cfg(feature = "client")]
    use crate::transport::AddrMaybeCached;
    #[cfg(feature = "client")]
    use std::time::Duration;
    #[cfg(feature = "client")]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    #[cfg(feature = "client")]
    use tokio::net::{TcpListener, TcpStream};
    #[cfg(feature = "client")]
    use url::Url;

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

    /// Read an HTTP request head without assuming where the proxy's reader
    /// stopped: the test is the server, so it owns the byte stream.
    #[cfg(feature = "client")]
    async fn read_http_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        loop {
            let byte = stream.read_u8().await.unwrap();
            head.push(byte);
            if head.ends_with(b"\r\n\r\n") {
                return head;
            }
            assert!(
                head.len() < 8 * 1024,
                "HTTP proxy head never ended: {head:?}"
            );
        }
    }

    /// A SOCKS5 proxy is dialed, its username/password sub-negotiation runs,
    /// and the CONNECT target is the service endpoint rather than the proxy.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn a_socks5_proxy_carries_the_service_destination() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();

            let mut greeting = [0u8; 2];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting[0], 5, "SOCKS version");
            let mut methods = vec![0u8; usize::from(greeting[1])];
            stream.read_exact(&mut methods).await.unwrap();
            assert!(
                methods.contains(&2),
                "the client must offer username/password auth: {methods:?}"
            );
            stream.write_all(&[5, 2]).await.unwrap();

            let mut auth = [0u8; 2];
            stream.read_exact(&mut auth).await.unwrap();
            assert_eq!(auth, [1, 5], "username/password sub-negotiation version");
            let mut username = vec![0u8; 5];
            stream.read_exact(&mut username).await.unwrap();
            assert_eq!(username, b"proxy");
            let mut password_len = [0u8; 1];
            stream.read_exact(&mut password_len).await.unwrap();
            let mut password = vec![0u8; usize::from(password_len[0])];
            stream.read_exact(&mut password).await.unwrap();
            assert_eq!(password, b"secret");
            stream.write_all(&[1, 0]).await.unwrap();

            let mut request = [0u8; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(request[..3], [5, 1, 0], "CONNECT request");
            assert_eq!(request[3], 3, "the target must travel as a domain name");
            let mut host_len = [0u8; 1];
            stream.read_exact(&mut host_len).await.unwrap();
            let mut host = vec![0u8; usize::from(host_len[0])];
            stream.read_exact(&mut host).await.unwrap();
            assert_eq!(host, b"example.com");
            let mut port = [0u8; 2];
            stream.read_exact(&mut port).await.unwrap();
            assert_eq!(u16::from_be_bytes(port), 443);

            stream
                .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
                .await
                .unwrap();
            stream.write_all(b"pong").await.unwrap();
        });

        let proxy = Url::parse(&format!("socks5://proxy:secret@{proxy_addr}")).unwrap();
        let target = AddrMaybeCached::new("example.com:443");
        let mut client = tokio::time::timeout(
            Duration::from_secs(5),
            tcp_connect_with_proxy(&target, Some(&proxy)),
        )
        .await
        .expect("SOCKS5 handshake timed out")
        .expect("SOCKS5 proxy dial failed");
        let mut pong = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut pong))
            .await
            .expect("reading through the proxy timed out")
            .unwrap();
        assert_eq!(&pong, b"pong");
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("proxy task timed out")
            .unwrap();
    }

    /// An HTTP CONNECT proxy receives the target and the Basic credential, and
    /// a 200 response is what hands the tunnel back to the caller.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn an_http_connect_proxy_carries_the_service_destination() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let head = read_http_head(&mut stream).await;
            let text = String::from_utf8_lossy(&head);
            assert!(
                text.starts_with("CONNECT example.com:443 HTTP/1.1\r\n"),
                "wrong CONNECT target: {text:?}"
            );
            assert!(
                text.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"),
                "the Basic credential was not sent: {text:?}"
            );
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
        });

        let proxy = Url::parse(&format!("http://user:pass@{proxy_addr}")).unwrap();
        let target = AddrMaybeCached::new("example.com:443");
        tokio::time::timeout(
            Duration::from_secs(5),
            tcp_connect_with_proxy(&target, Some(&proxy)),
        )
        .await
        .expect("HTTP CONNECT timed out")
        .expect("HTTP CONNECT proxy dial failed");
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("proxy task timed out")
            .unwrap();
    }

    /// The IPv6-preferring UDP lookup falls back to IPv4 when the host has
    /// only an A record; the connected socket then carries a datagram.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn udp_connect_prefers_ipv6_but_falls_back_to_ipv4() {
        let receiver = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let receiver_addr = receiver.local_addr().unwrap();
        let sender = udp_connect(("127.0.0.1", receiver_addr.port()), true)
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

    /// With no proxy configured the helper is a plain direct dial; this pins
    /// that the proxy branch did not become mandatory.
    #[cfg(feature = "client")]
    #[tokio::test]
    async fn without_a_proxy_the_helper_dials_directly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(b"pong").await.unwrap();
        });

        let target = AddrMaybeCached::new(&addr.to_string());
        let mut client = tcp_connect_with_proxy(&target, None)
            .await
            .expect("direct dial failed");
        let mut pong = [0u8; 4];
        client.read_exact(&mut pong).await.unwrap();
        assert_eq!(&pong, b"pong");
        server.await.unwrap();
    }
}
