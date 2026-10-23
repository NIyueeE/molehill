//! A direct data channel, and a pool tunnel's connection, on the UDP carrier.
//!
//! A direct channel is one connection carrying one stream — the same shape a
//! KCP session has today — so this owns the endpoint that carries it: the
//! channel holds the endpoint handle, and dropping the channel closes the
//! socket and the task with it.
//!
//! A pool tunnel is the other shape (one endpoint, many connections, many
//! streams), and it is deliberately not built here: this is the lane path, and
//! a lane is a connection.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use quinn_proto::ClientConfig;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::conn::QuicStream;
use super::session::{NoiseClientConfig, NoiseServerConfig};
use super::socket::{self, Channels, EndpointHandle};
use crate::transport::NoiseKeys;

/// One direct data channel: the stream, and the endpoint that carries it.
pub(crate) struct UdpChannel {
    /// Kept for the channel's whole life: it owns the socket and its task.
    _endpoint: Arc<EndpointHandle>,
    stream: QuicStream,
}

/// Dial one direct data channel to `remote`.
///
/// The bind address follows the endpoint's family, and the socket is a fresh
/// one per channel, exactly as a KCP session is today: the carrier's own
/// connection is the unit, and nothing above it shares one yet.
///
/// # Errors
///
/// Fails when the socket cannot be bound, the handshake does not complete, or
/// the task is gone.
pub(crate) async fn connect(
    remote: SocketAddr,
    keys: NoiseKeys,
    encrypt: bool,
) -> Result<UdpChannel> {
    let endpoint = dial_endpoint(remote).await?;
    let mut config = ClientConfig::new(Arc::new(NoiseClientConfig::new(keys)));
    config.transport_config(super::params::transport_config());
    let stream = endpoint.connect_stream(remote, config, encrypt).await?;
    spawn_stats_reporter(&stream);
    Ok(UdpChannel {
        _endpoint: endpoint,
        stream,
    })
}

/// Log one line per second per connection while `MOLEHILL_QUIC_STATS` is set:
/// the engine's own view of the path, which is what tells a loss-collapse
/// apart from a stalled reader. Both ends start one for a connection they
/// accept or dial, because a collapse shows on the side that is *not* sending
/// as much as on the one that is.
pub(crate) fn spawn_stats_reporter(stream: &super::conn::QuicStream) {
    if std::env::var_os("MOLEHILL_QUIC_STATS").is_none() {
        return;
    }
    let key = stream.connection_key();
    let shared = stream.shared_weak();
    // The key is only unique inside one driver, and a process has one driver
    // per socket: the connection's own address is what tells two lanes apart
    // in a merged log.
    let id = std::sync::Weak::as_ptr(&shared) as usize;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            ticker.tick().await;
            let Some(shared) = shared.upgrade() else {
                break;
            };
            let (rtt, cwnd, lost, sent, streams) = shared.engine_stats();
            tracing::info!(
                target: "molehill::quic",
                "quic-stats: id={id:x} key={key} rtt_ms={} cwnd={} lost={} sent={} streams={}",
                rtt.as_millis(),
                cwnd,
                lost,
                sent,
                streams
            );
        }
    });
}

/// Dial one **connection** to `remote`, for a carrier that will open many
/// streams over it (the pool's tunnels).
///
/// The connection is not handed back as a concrete stream: the caller keeps
/// the seam handle and opens streams through it, which is the same shape the
/// pool uses for every carrier.
///
/// # Errors
///
/// Fails when the socket cannot be bound or the endpoint cannot start the
/// connection. The handshake completes lazily, on the first open, exactly as
/// it does for a direct channel.
pub(crate) async fn connect_connection(
    remote: SocketAddr,
    keys: NoiseKeys,
    encrypt: bool,
) -> Result<Arc<dyn crate::transport::carrier::CarrierConn>> {
    let endpoint = dial_endpoint(remote).await?;
    let mut config = ClientConfig::new(Arc::new(NoiseClientConfig::new(keys)));
    config.transport_config(super::params::transport_config());
    let handle = endpoint.connect(remote, config).await?;
    Ok(Arc::new(super::seam::UdpConn::new(
        endpoint, handle, encrypt,
    )))
}

/// One client endpoint on a fresh socket, with its task's receive channels
/// drained: a client accepts nothing, and a send that fails because a receiver
/// is gone would end the task.
async fn dial_endpoint(remote: SocketAddr) -> Result<Arc<EndpointHandle>> {
    let bind = if remote.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    let socket = UdpSocket::bind(bind)
        .await
        .with_context(|| format!("Failed to bind a UDP socket for the data endpoint {remote}"))?;
    let (accepted, mut accepted_rx) = mpsc::unbounded_channel();
    let (events, mut events_rx) = mpsc::unbounded_channel();
    let (streams, mut streams_rx) = mpsc::unbounded_channel();
    let (lost, mut lost_rx) = mpsc::unbounded_channel();
    let (endpoint, _task) = socket::spawn(
        socket,
        None,
        false,
        Channels {
            accepted,
            events,
            streams,
            lost,
        },
    );
    // A client endpoint accepts nothing: the server opens neither connections
    // nor streams towards it. The receivers are drained rather than dropped so
    // the task's sends stay cheap and cannot fail under it.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = accepted_rx.recv() => {
                    if accepted.is_none() {
                        break;
                    }
                }
                event = events_rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                }
                stream = streams_rx.recv() => {
                    if stream.is_none() {
                        break;
                    }
                }
                lost = lost_rx.recv() => {
                    if lost.is_none() {
                        break;
                    }
                }
            }
        }
    });
    Ok(Arc::new(endpoint))
}

impl tokio::io::AsyncRead for UdpChannel {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for UdpChannel {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// Bind a **server** endpoint: one socket, one task, many connections.
///
/// The engine's own configuration type stops here, like it does on the client
/// side, and so do the channels a caller has no use for: a direct channel
/// arrives as an accepted *stream*, which is what the accept loop holds.
pub(crate) fn spawn_server(
    socket: UdpSocket,
    keys: NoiseKeys,
) -> (
    Arc<EndpointHandle>,
    mpsc::UnboundedReceiver<QuicStream>,
    mpsc::UnboundedReceiver<u64>,
) {
    let (accepted_tx, mut accepted_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (streams_tx, streams_rx) = mpsc::unbounded_channel();
    let (lost_tx, lost_rx) = mpsc::unbounded_channel();
    let mut config = quinn_proto::ServerConfig::new(
        Arc::new(NoiseServerConfig::new(keys)),
        super::crypto::TokenKeys::new(),
    );
    config.transport_config(super::params::transport_config());
    let (endpoint, _task) = socket::spawn(
        socket,
        Some(Arc::new(config)),
        false,
        Channels {
            lost: lost_tx,
            accepted: accepted_tx,
            events: events_tx,
            streams: streams_tx,
        },
    );
    // Accepted connections and their events belong to the pool path, which
    // reads streams instead; draining them keeps the task's sends cheap and
    // failing-free.
    tokio::spawn(async move {
        loop {
            tokio::select! {
                accepted = accepted_rx.recv() => {
                    if accepted.is_none() {
                        break;
                    }
                }
                event = events_rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                }
            }
        }
    });
    (Arc::new(endpoint), streams_rx, lost_rx)
}
