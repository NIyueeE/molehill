//! A pool-less `mhd` lane pair: the carrier, with nothing above it.
//!
//! The bench asks two different questions and they must not be answered with one
//! number. *What does the product give a visitor?* is the tunnel pool, the
//! tunnel hello and its acknowledgement, the stripe placement, the carrier and
//! the backend, end to end. *What does the carrier itself cost?* is the engine in
//! [`super::mhd`] — its socket pump, dispatcher, packet-number space, credit and
//! recovery — with none of the rest. A run that measures only the first cannot
//! say which layer a millisecond belongs to; that ambiguity is what this module
//! exists to remove (`MHD-FINALIZATION-PLAN.md`, phase 0).
//!
//! The surface is deliberately the smallest one that answers the second
//! question: a lane that dials or accepts, a stream that opens, and the streams
//! the peer opens. Nothing here decides anything the engine does not already
//! decide.
//!
//! # What this seam does **not** measure
//!
//! [`Lane::open`] is the carrier's own open: the transport selector byte and
//! nothing after it. It is **not** the product's stream open. The product's
//! `PoolLane::open_stream` writes a `DataChannelTunnelHello` and waits for the
//! server's `Ack::Ok`, and the real server only answers that after
//! `reserve_tunnel` has matched the hello's nonce against a live session — a
//! session no bare probe can create. Reaching it from here would mean either
//! re-implementing the server's registration (a second implementation to
//! measure, which is worthless) or lifting `core::server`'s handshake into a
//! shared library function first (a deliberate refactor, not a probe). So the
//! tunnel open's round trip is measured on the product path — `churn-16`, the
//! staged timeline, and the open-phase histograms — and the carrier's open is
//! measured here. The two are reported under different metric names for that
//! reason, and a reader who conflates them would attribute the hello's cost to
//! the engine.

use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::transport::mhd::{MhdLane, MhdListener};
use crate::transport::multiplex::{CarrierStream, LaneError, PoolLane};

/// Why the carrier refused an open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// The lane's connection ended: its pump stopped, or its peer went silent for
    /// longer than the dead-path limit.
    Closed,
    /// The lane is at its stream ceiling right now. Distinct from [`Self::Closed`]
    /// because it is a capacity answer, and a harness that pooled the two would
    /// report a full lane as a dead one.
    Busy,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("the lane is gone"),
            Self::Busy => f.write_str("the lane is at its stream ceiling"),
        }
    }
}

impl std::error::Error for Refused {}

/// One carrier stream, as the probe hands it out.
///
/// A newtype rather than `Box<dyn AsyncRead + AsyncWrite>` because that type is
/// not object-safe as written and a second box to make it so would add one
/// indirection to every read and write — on a probe whose whole subject is
/// latency, that is a measurement defect, not a detail.
pub struct Io(Inner);

/// Which side of the lane a [`Io`] came from.
///
/// Carried because the two arrive through different engine paths — an opened
/// stream is the caller's own id, an accepted one was minted by the peer — and a
/// harness that had to guess would be one confusion away from reporting the two
/// directions' costs as one.
enum Inner {
    /// A stream this end opened.
    Opened(CarrierStream),
    /// A stream the peer opened.
    Accepted(crate::transport::mhd::lane::StreamIo),
}

impl std::fmt::Debug for Io {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Inner::Opened(_) => "Io::Opened",
            Inner::Accepted(_) => "Io::Accepted",
        })
    }
}

impl AsyncRead for Io {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().0 {
            Inner::Opened(stream) => Pin::new(stream).poll_read(cx, buf),
            Inner::Accepted(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match &mut self.get_mut().0 {
            Inner::Opened(stream) => Pin::new(stream).poll_write(cx, buf),
            Inner::Accepted(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().0 {
            Inner::Opened(stream) => Pin::new(stream).poll_flush(cx),
            Inner::Accepted(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match &mut self.get_mut().0 {
            Inner::Opened(stream) => Pin::new(stream).poll_shutdown(cx),
            Inner::Accepted(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

/// One carrier connection, dialed or accepted, without a pool around it.
#[derive(Debug)]
pub struct Lane(Arc<MhdLane>);

impl Lane {
    /// Dial a lane at `peer`.
    ///
    /// It takes no id on purpose. The number that matters — the one the peer
    /// keys its connection table by — is the engine's own and unique per process
    /// (`pump::next_wire_id`). The pool label a caller used to pass here only
    /// ever named a lane in a log, and exposing it invited exactly the collision
    /// it could not prevent (`HANDOFF.md` §4.100).
    ///
    /// The nonce is zero: a bare lane carries no tunnel hello, so it names no
    /// session — see the module docs for why that is the honest boundary rather
    /// than an omission.
    ///
    /// # Errors
    ///
    /// Fails when the local UDP socket cannot be bound.
    pub async fn dial(peer: SocketAddr) -> std::io::Result<Arc<Self>> {
        let lane =
            MhdLane::dial(peer, 0, None, [0u8; crate::protocol::HASH_WIDTH_IN_BYTES]).await?;
        Ok(Arc::new(Self(lane)))
    }

    /// Open one stream: the transport selector, then the caller's bytes.
    ///
    /// # Errors
    ///
    /// Answers [`Refused`] when the lane's connection ended or the lane is at its
    /// stream ceiling.
    pub async fn open(&self) -> Result<Io, Refused> {
        match self.0.open_channel().await {
            Ok(stream) => Ok(Io(Inner::Opened(stream))),
            Err(LaneError::Busy) => Err(Refused::Busy),
            Err(LaneError::Closed) => Err(Refused::Closed),
        }
    }

    /// Take the next stream the peer opened, if one is waiting.
    ///
    /// `None` means the lane ended: a lane that is alive and idle answers
    /// nothing here, so a caller that must tell the two apart checks
    /// [`Self::is_alive`] — which is exactly the discrimination a measurement
    /// needs when a probe stops.
    pub async fn accept(&self) -> Option<Io> {
        self.0
            .wait_stream()
            .await
            .map(|stream| Io(Inner::Accepted(stream)))
    }

    /// Whether the lane's connection is still up.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        PoolLane::is_alive(&*self.0)
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        // The engine's pump task holds only the shared state, so the connection
        // outlives this handle by design; asking for a graceful close lets the
        // probe's last queued stream finish before the socket goes.
        PoolLane::mark_dead(&*self.0);
    }
}

/// A probe's listening end: one socket set, one dispatcher per socket.
pub struct Listener(MhdListener);

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("addr", &self.0.local_addr())
            .finish_non_exhaustive()
    }
}

impl Listener {
    /// Bind the listener at `addr`.
    ///
    /// # Errors
    ///
    /// Fails when no listening socket can be bound.
    pub fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        MhdListener::bind(addr).map(Self)
    }

    /// The address it bound — the port a probe must dial, which is why it is read
    /// rather than assumed when the caller asked for port 0.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.0.local_addr()
    }

    /// Accept the next lane a peer opens, or `None` once the listener is done.
    pub async fn accept(&mut self) -> Option<Arc<Lane>> {
        self.0.accept().await.map(|lane| Arc::new(Lane(lane)))
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]

    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The seam in one test: a lane with no pool around it carries bytes both
    /// ways, and the first byte the peer reads is the selector the open wrote.
    ///
    /// The selector assertion is the load-bearing one. A probe that silently
    /// differed from the product's open — one that skipped the selector, or wrote
    /// a different one — would measure a path the product never takes, and every
    /// number it produced afterwards would be about that path.
    #[tokio::test]
    async fn a_pool_less_lane_carries_bytes_both_ways() {
        let mut listener = Listener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr();
        let dialer = Lane::dial(addr).await.unwrap();

        let server = tokio::spawn(async move {
            let lane = listener.accept().await.unwrap();
            let mut io = lane.accept().await.unwrap();
            let mut selector = [0u8; 1];
            io.read_exact(&mut selector).await.unwrap();
            assert_eq!(
                selector[0],
                crate::protocol::PLAIN_SELECTOR,
                "the open must write the product's plain selector"
            );
            let mut got = [0u8; 5];
            io.read_exact(&mut got).await.unwrap();
            io.write_all(&got).await.unwrap();
            io.flush().await.unwrap();
        });

        let mut io = dialer.open().await.unwrap();
        io.write_all(b"hello").await.unwrap();
        io.flush().await.unwrap();
        let mut back = [0u8; 5];
        tokio::time::timeout(Duration::from_secs(10), io.read_exact(&mut back))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&back, b"hello", "the carrier is byte-exact");
        drop(io);
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap();
    }
}
