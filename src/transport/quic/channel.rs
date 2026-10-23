//! A direct data channel on the UDP carrier.
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
    let (endpoint, _task) = socket::spawn(
        socket,
        None,
        false,
        Channels {
            accepted,
            events,
            streams,
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
            }
        }
    });
    let endpoint = Arc::new(endpoint);
    let config = ClientConfig::new(Arc::new(NoiseClientConfig::new(keys)));
    let stream = endpoint.connect_stream(remote, config, encrypt).await?;
    Ok(UdpChannel {
        _endpoint: endpoint,
        stream,
    })
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
) -> (Arc<EndpointHandle>, mpsc::UnboundedReceiver<QuicStream>) {
    let (accepted_tx, mut accepted_rx) = mpsc::unbounded_channel();
    let (events_tx, mut events_rx) = mpsc::unbounded_channel();
    let (streams_tx, streams_rx) = mpsc::unbounded_channel();
    let config = quinn_proto::ServerConfig::new(
        Arc::new(NoiseServerConfig::new(keys)),
        super::crypto::TokenKeys::new(),
    );
    let (endpoint, _task) = socket::spawn(
        socket,
        Some(Arc::new(config)),
        false,
        Channels {
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
    (Arc::new(endpoint), streams_rx)
}
