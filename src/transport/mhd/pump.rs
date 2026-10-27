//! The socket pump: one task per connection that moves datagrams between the
//! state machine and the UDP socket.
//!
//! It is deliberately dumb. Every pass first drains whatever the connection
//! wants to send, then waits for input; the wait is bounded so a state change
//! made by a writer (a new range queued, a credit update) is noticed without the
//! pump having to be woken from outside. A readiness-driven pump belongs with
//! the pool attachment, where the wake-up path is designed once for both.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use super::lane::{Shared, StreamIo};

/// How long one pass waits for a datagram before re-checking what it owes the
/// peer.
const IDLE_WAIT: Duration = Duration::from_millis(2);

/// Bind the local socket of a connection.
pub(crate) async fn bind() -> std::io::Result<Arc<UdpSocket>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    Ok(Arc::new(socket))
}

/// Start the pump for one connection.
pub(crate) fn spawn(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut out = [0u8; 2048];
        let mut inbox = [0u8; 2048];
        loop {
            // 1. Everything the connection wants to say.
            loop {
                let now = Instant::now();
                let packet = shared.with(|conn| conn.poll_transmit(now, &mut out));
                let Some(len) = packet else { break };
                if socket.send_to(&out[..len], peer).await.is_err() {
                    return;
                }
            }
            // 2. Whatever the peer has said, bounded so a local write is not
            //    left waiting for a silent socket.
            match tokio::time::timeout(IDLE_WAIT, socket.recv_from(&mut inbox)).await {
                Ok(Ok((len, _))) => {
                    let now = Instant::now();
                    shared.with(|conn| {
                        let _ = conn.on_datagram(now, &inbox[..len]);
                    });
                    shared.wake_readers();
                    shared.wake_writers();
                }
                Ok(Err(_)) => return,
                Err(_elapsed) => {}
            }
            shared.wake_pump();
        }
    })
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use crate::transport::mhd::conn::Conn;
    use crate::transport::mhd::lane::StreamIo;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One stream across a real pair of loopback sockets: the pump, the wire
    /// format, the connection and the seam, with nothing faked between them.
    #[tokio::test]
    async fn a_stream_crosses_two_loopback_sockets() {
        let dialer = Shared::new(Conn::new(21, true));
        let acceptor = Shared::new(Conn::new(21, false));
        let dialer_socket = bind().await.unwrap();
        let acceptor_socket = bind().await.unwrap();
        let dialer_addr = dialer_socket.local_addr().unwrap();
        let acceptor_addr = acceptor_socket.local_addr().unwrap();
        let _a = spawn(
            Arc::clone(&dialer_socket),
            acceptor_addr,
            Arc::clone(&dialer),
        );
        let _b = spawn(
            Arc::clone(&acceptor_socket),
            dialer_addr,
            Arc::clone(&acceptor),
        );

        let id = dialer.with(|conn| conn.open().id);
        let mut writer = StreamIo::new(Arc::clone(&dialer), id);
        tokio::spawn(async move {
            writer.write_all(b"across the wire").await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut peer_id = None;
        for _ in 0..2000 {
            if let Some(handle) = acceptor.with(crate::transport::mhd::conn::Conn::accept) {
                peer_id = Some(handle.id);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let Some(peer_id) = peer_id else {
            unreachable!("the peer must see the stream")
        };
        let mut reader = StreamIo::new(Arc::clone(&acceptor), peer_id);
        let mut got = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), reader.read_to_end(&mut got)).await;
        let Ok(Ok(_)) = read else {
            unreachable!("the read must finish: got {got:?}")
        };
        assert_eq!(got, b"across the wire");
    }
}

/// One connection as the pool holds it: a shared state machine, the counters
/// its streams are charged to, and the socket its pump sends on.
#[derive(Debug)]
pub(crate) struct MhdLane {
    id: usize,
    shared: Arc<Shared>,
    counters: Arc<crate::transport::multiplex::TunnelCounters>,
    /// Dropped with the lane: the pump task ends on the next failed send.
    _task: tokio::task::JoinHandle<()>,
}

impl MhdLane {
    /// Dial a connection to a listening endpoint.
    pub(crate) async fn connect(peer: SocketAddr, id: usize) -> std::io::Result<Arc<Self>> {
        let socket = bind().await?;
        let shared = Shared::new(crate::transport::mhd::conn::Conn::new(
            u32::try_from(id).unwrap_or(1),
            true,
        ));
        let task = spawn(Arc::clone(&socket), peer, Arc::clone(&shared));
        Ok(Arc::new(Self {
            id,
            shared,
            counters: crate::transport::multiplex::TunnelCounters::for_lane(),
            _task: task,
        }))
    }

    /// Take a stream the peer opened, if one is waiting.
    pub(crate) fn accept_stream(&self) -> Option<StreamIo> {
        let id = self
            .shared
            .with(|conn| conn.accept().map(|handle| handle.id))?;
        Some(StreamIo::new(Arc::clone(&self.shared), id))
    }
}
#[async_trait::async_trait]
impl crate::transport::multiplex::PoolLane for MhdLane {
    fn id(&self) -> usize {
        self.id
    }

    fn counters(&self) -> &Arc<crate::transport::multiplex::TunnelCounters> {
        &self.counters
    }

    fn is_alive(&self) -> bool {
        self.shared.with(|conn| conn.closed().is_none())
    }

    fn mark_dead(&self) {
        self.shared
            .with(|conn| conn.close(crate::transport::mhd::wire::ResetCode::Aborted));
    }

    async fn open_stream(
        &self,
    ) -> Result<crate::transport::multiplex::CarrierStream, crate::transport::multiplex::LaneError>
    {
        if !self.is_alive() {
            return Err(crate::transport::multiplex::LaneError::Closed);
        }
        let id = self.shared.with(|conn| conn.open().id);
        self.shared.wake_pump();
        Ok(Box::new(StreamIo::new(Arc::clone(&self.shared), id)))
    }
}

/// A pump that only sends: the listener owns the socket's receive side and
/// routes each datagram to the connection it names, so an accepted lane must not
/// read from it as well.
fn spawn_sender(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut out = [0u8; 2048];
        loop {
            loop {
                let now = Instant::now();
                let packet = shared.with(|conn| conn.poll_transmit(now, &mut out));
                let Some(len) = packet else { break };
                if socket.send_to(&out[..len], peer).await.is_err() {
                    return;
                }
            }
            tokio::time::sleep(IDLE_WAIT).await;
            if shared.with(|conn| conn.closed().is_some()) {
                return;
            }
        }
    })
}

/// The listening end: one socket, one connection per id it has never seen, and
/// one dispatcher that routes.
#[derive(Debug)]
pub(crate) struct MhdListener {
    addr: SocketAddr,
    incoming: tokio::sync::mpsc::UnboundedReceiver<Arc<MhdLane>>,
}

impl MhdListener {
    /// Bind the listener and start its dispatcher.
    pub(crate) async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let socket = Arc::new(UdpSocket::bind(addr).await?);
        let bound = socket.local_addr()?;
        let (tx, incoming) = tokio::sync::mpsc::unbounded_channel();
        let registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let next_id = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let mut buf = vec![0u8; 2048];
        tokio::spawn(async move {
            loop {
                let Ok((len, from)) = socket.recv_from(&mut buf).await else {
                    return;
                };
                let Ok(packet) = crate::transport::mhd::wire::decode(&buf[..len]) else {
                    continue;
                };
                let now = Instant::now();
                if let Some(shared) = registry.lock().get(&packet.id).cloned() {
                    shared.with(|conn| {
                        let _ = conn.on_datagram(now, &buf[..len]);
                    });
                    shared.wake_readers();
                    shared.wake_writers();
                    continue;
                }
                let shared = Shared::new(crate::transport::mhd::conn::Conn::new(packet.id, false));
                shared.with(|conn| {
                    let _ = conn.on_datagram(now, &buf[..len]);
                });
                registry.lock().insert(packet.id, Arc::clone(&shared));
                let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let task = spawn_sender(Arc::clone(&socket), from, Arc::clone(&shared));
                let lane = Arc::new(MhdLane {
                    id,
                    shared,
                    counters: crate::transport::multiplex::TunnelCounters::for_lane(),
                    _task: task,
                });
                if tx.send(lane).is_err() {
                    return;
                }
            }
        });
        Ok(Self {
            addr: bound,
            incoming,
        })
    }

    /// The address it bound.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The next connection a peer opens.
    ///
    /// A datagram naming an id this end has never seen starts the connection:
    /// there is no handshake to wait for, which is the point of the carrier.
    pub(crate) async fn accept(&mut self) -> Option<Arc<MhdLane>> {
        self.incoming.recv().await
    }
}

#[cfg(test)]
mod lane_tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use crate::transport::multiplex::PoolLane;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The pool's own view: a dialing lane and a listening lane, one stream
    /// opened through the trait the pool uses, read on the other side.
    #[tokio::test]
    async fn a_lane_opens_a_stream_the_listener_accepts() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).await.unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::connect(bound, 3).await.unwrap();

        let mut stream = PoolLane::open_stream(&*dialer).await.unwrap();
        tokio::spawn(async move {
            stream.write_all(b"lane to lane").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut inbound = None;
        for _ in 0..2000 {
            if let Some(stream) = accepted.accept_stream() {
                inbound = Some(stream);
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let Some(mut inbound) = inbound else {
            unreachable!("the listener must accept the stream")
        };
        let mut got = Vec::new();
        let read =
            tokio::time::timeout(Duration::from_secs(5), inbound.read_to_end(&mut got)).await;
        let Ok(Ok(_)) = read else {
            unreachable!("the read must finish: got {got:?}")
        };
        assert_eq!(got, b"lane to lane");
        assert!(PoolLane::is_alive(&*accepted));
    }
}
