#[cfg(feature = "server")]
use anyhow::Context;
use anyhow::{Result, anyhow};
use async_http_proxy::{http_connect_tokio, http_connect_tokio_with_basic_auth};
#[cfg(feature = "server")]
use backon::Retryable;
use socket2::{SockRef, TcpKeepalive};
#[cfg(feature = "server")]
use std::future::Future;
#[cfg(feature = "client")]
use std::net::SocketAddr;
use std::time::Duration;
#[cfg(feature = "server")]
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
#[cfg(feature = "client")]
use tokio::net::{ToSocketAddrs, UdpSocket, lookup_host};
#[cfg(feature = "server")]
use tokio::sync::broadcast;
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

#[allow(dead_code)]
pub fn feature_not_compile(feature: &str) -> ! {
    eprintln!(
        "The feature '{}' is not compiled in this binary. Please re-compile molehill",
        feature
    );
    std::process::exit(1);
}

#[allow(dead_code)]
pub fn feature_neither_compile(feature1: &str, feature2: &str) -> ! {
    eprintln!(
        "Neither of the feature '{}' or '{}' is compiled in this binary. Please re-compile molehill",
        feature1, feature2
    );
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
        .ok_or_else(|| anyhow!("Address is missing the port: {}", s))?;
    Ok((&s[..semi], s[semi + 1..].parse()?))
}

#[cfg(feature = "client")]
/// Create a UDP socket and connect to `addr`
pub async fn udp_connect<A: ToSocketAddrs>(addr: A, prefer_ipv6: bool) -> Result<UdpSocket> {
    let (socket_addr, bind_addr);

    match prefer_ipv6 {
        false => {
            socket_addr = to_socket_addr(addr).await?;

            bind_addr = match socket_addr {
                SocketAddr::V4(_) => "0.0.0.0:0",
                SocketAddr::V6(_) => ":::0",
            };
        }
        true => {
            let all_host_addresses: Vec<SocketAddr> = lookup_host(addr).await?.collect();

            // Try to find an IPv6 address
            match all_host_addresses.clone().iter().find(|x| x.is_ipv6()) {
                Some(socket_addr_ipv6) => {
                    socket_addr = *socket_addr_ipv6;
                    bind_addr = ":::0";
                }
                None => {
                    let socket_addr_ipv4 = all_host_addresses.iter().find(|x| x.is_ipv4());
                    match socket_addr_ipv4 {
                        None => return Err(anyhow!("Failed to lookup the host")),
                        // fallback to IPv4
                        Some(socket_addr_ipv4) => {
                            socket_addr = *socket_addr_ipv4;
                            bind_addr = "0.0.0.0:0";
                        }
                    }
                }
            }
        }
    };
    let s = UdpSocket::bind(bind_addr).await?;
    s.connect(socket_addr).await?;
    Ok(s)
}

/// Create a TcpStream using a proxy
/// e.g. socks5://user:pass@127.0.0.1:1080 http://127.0.0.1:8080
pub async fn tcp_connect_with_proxy(
    addr: &AddrMaybeCached,
    proxy: Option<&Url>,
) -> Result<TcpStream> {
    if let Some(url) = proxy {
        let addr = &addr.addr;
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("Proxy URL is missing the host: {}", url))?;
        let port = url
            .port()
            .ok_or_else(|| anyhow!("Proxy URL is missing the port: {}", url))?;
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
                        .await?
                    }
                    None => http_connect_tokio(&mut s, host, port).await?,
                }
            }
            scheme => return Err(anyhow!("Unknown proxy scheme: {}", scheme)),
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
pub async fn retry_notify_with_deadline<I, E, Op, Fut, B, N>(
    backoff: B,
    operation: Op,
    notify: N,
    deadline: &mut broadcast::Receiver<bool>,
) -> Result<I>
where
    E: std::error::Error + Send + Sync + 'static,
    B: backon::BackoffBuilder,
    Op: Fn() -> Fut,
    Fut: Future<Output = std::result::Result<I, E>>,
    N: Fn(&E, Duration) + Send + Sync,
{
    tokio::select! {
        v = operation.retry(backoff).notify(notify) => {
            v.map_err(anyhow::Error::new)
        }
        _ = deadline.recv() => {
            Err(anyhow!("shutdown"))
        }
    }
}

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
