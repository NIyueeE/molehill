#[cfg(feature = "client")]
use crate::common::helper::to_socket_addr;
use crate::common::helper::try_set_tcp_keepalive;
use crate::config::TransportConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use std::fmt::{Debug, Display};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpStream, ToSocketAddrs};
use tracing::{error, trace};

pub const DEFAULT_KEEPALIVE_SECS: u64 = 20;
pub const DEFAULT_KEEPALIVE_INTERVAL: u64 = 8;

#[derive(Clone)]
#[doc(hidden)]
pub struct AddrMaybeCached {
    pub addr: String,
    pub socket_addr: Option<SocketAddr>,
}

impl AddrMaybeCached {
    #[cfg(feature = "client")]
    #[must_use]
    pub fn new(addr: &str) -> AddrMaybeCached {
        AddrMaybeCached {
            addr: addr.to_string(),
            socket_addr: None,
        }
    }

    #[cfg(feature = "client")]
    /// Resolve the address to one socket address.
    ///
    /// # Errors
    ///
    /// Fails when the name does not resolve.
    pub async fn resolve(&mut self) -> Result<()> {
        match to_socket_addr(&self.addr).await {
            Ok(s) => {
                self.socket_addr = Some(s);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}

impl Display for AddrMaybeCached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.socket_addr {
            Some(s) => f.write_fmt(format_args!("{s}")),
            None => f.write_str(&self.addr),
        }
    }
}

/// Specify a transport layer: plain TCP or Noise.
#[async_trait]
#[doc(hidden)]
pub trait Transport: Debug + Send + Sync {
    type Acceptor: Send + Sync;
    type RawStream: Send + Sync;
    type Stream: 'static + AsyncRead + AsyncWrite + Unpin + Send + Sync + Debug;

    fn new(config: &TransportConfig) -> Result<Self>
    where
        Self: Sized;
    /// Bind a listener (server side only).
    #[cfg(feature = "server")]
    async fn bind<T: ToSocketAddrs + Send + Sync>(&self, addr: T) -> Result<Self::Acceptor>;
    /// Accept a connection; must be cancel safe (server side only).
    #[cfg(feature = "server")]
    async fn accept(&self, a: &Self::Acceptor) -> Result<(Self::RawStream, SocketAddr)>;
    /// Dial a connection (client side only).
    #[cfg(feature = "client")]
    async fn connect(&self, addr: &AddrMaybeCached) -> Result<Self::Stream>;
}

mod tcp;
pub use tcp::TcpTransport;

/// The data plane's carrier and the fallback that chooses it (KCP, else TCP).
///
/// `carrier` is independent of yamux: a transparent claim uses the carrier
/// choice without ever multiplexing its traffic, so the type and policy are a
/// feature of their own; `multiplex` includes them for the tunnel pool.
pub mod health;

#[cfg(feature = "carrier")]
pub mod carrier;

#[cfg(feature = "noise")]
mod noise;
#[cfg(feature = "noise")]
pub use noise::NoiseTransport;
// The u16-framed Noise record stream over tokio IO (vendored from
// snowstorm 0.4.0, adapted to snow 0.10 — see noise_stream.rs).
#[cfg(feature = "noise")]
pub(crate) mod noise_stream;
#[cfg(feature = "noise")]
pub use noise_stream::NoiseStream;
// Key material: the Noise transport, the server's dual-transport accept,
// and the KCP tunnel path (Noise-over-KCP) all build sessions from it.
#[cfg(feature = "noise")]
pub(crate) use noise::NoiseKeys;

#[cfg(all(feature = "kcp", any(feature = "client", feature = "server")))]
pub(crate) mod kcp;

/// The portable half of an outbound datagram batch: the batch's shape, which
/// every platform shares, as opposed to the syscall that sends it. Gated with
/// `kcp` and not with the platform, because the non-Linux send path uses it.
#[cfg(all(feature = "kcp", any(feature = "client", feature = "server")))]
pub(crate) mod dgram;

// Batch UDP datagram IO (recvmmsg/sendmmsg) for the KCP carrier on Linux.
#[cfg(all(
    target_os = "linux",
    feature = "kcp",
    any(feature = "client", feature = "server")
))]
pub(crate) mod udp_batch;

#[cfg(feature = "multiplex")]
pub(crate) mod multiplex;
#[cfg(feature = "multiplex")]
pub(crate) use multiplex::MuxStream;
// The pinned pool's *policy*: internal constants, placement arithmetic and
// the stream ceiling. Pure, so the rules are unit-tested without a socket.
#[cfg(feature = "multiplex")]
pub(crate) mod pool;
// The client's tunnel pool and its per-session/per-service keys are re-exported
// for the integration suite, which asserts on the pool's own state instead of
// guessing it from the telemetry text.
#[cfg(feature = "multiplex")]
#[doc(hidden)]
pub use multiplex::{PoolSnapshot, TunnelPool};

#[derive(Debug, Clone, Copy)]
struct Keepalive {
    // tcp_keepalive_time if the underlying protocol is TCP
    pub keepalive_secs: u64,
    // tcp_keepalive_intvl if the underlying protocol is TCP
    pub keepalive_interval: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct SocketOpts {
    // None means do not change
    nodelay: Option<bool>,
    // keepalive must be Some or None at the same time, or the behavior will be platform-dependent
    keepalive: Option<Keepalive>,
}

impl SocketOpts {
    pub(crate) fn none() -> SocketOpts {
        SocketOpts {
            nodelay: None,
            keepalive: None,
        }
    }

    /// Socket options for the control channel
    pub fn for_control_channel() -> SocketOpts {
        SocketOpts {
            nodelay: Some(true),  // Always set nodelay for the control channel
            ..SocketOpts::none()  // None means do not change. Keepalive is set by TcpTransport
        }
    }
}

impl SocketOpts {
    /// Socket options for the legs of a forwarded service: the data channels
    /// on both ends and, on the server, the visitor-facing sockets.
    ///
    /// Latency-friendly and fixed: `TCP_NODELAY` is always on — otherwise
    /// interactive traffic like SSH suffers Nagle x delayed-ACK stalls — and
    /// TCP keepalive is enabled so pooled idle data channels don't hand out
    /// silently-dead connections to visitors.
    pub fn for_service() -> SocketOpts {
        SocketOpts {
            nodelay: Some(true),
            keepalive: Some(Keepalive {
                keepalive_secs: DEFAULT_KEEPALIVE_SECS,
                keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            }),
        }
    }

    pub fn apply(&self, conn: &TcpStream) {
        if let Some(v) = self.keepalive {
            let keepalive_duration = Duration::from_secs(v.keepalive_secs);
            let keepalive_interval = Duration::from_secs(v.keepalive_interval);

            if let Err(e) = try_set_tcp_keepalive(conn, keepalive_duration, keepalive_interval)
                .with_context(|| "Failed to set keepalive")
            {
                error!("{:#}", e);
            }
        }

        if let Some(nodelay) = self.nodelay {
            trace!("Set nodelay {}", nodelay);
            if let Err(e) = conn
                .set_nodelay(nodelay)
                .with_context(|| "Failed to set nodelay")
            {
                error!("{:#}", e);
            }
        }
    }
}
