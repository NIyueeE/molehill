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
use std::time::Instant;

use tokio::net::UdpSocket;

use super::lane::{Shared, StreamIo};

/// Bind the local socket of a connection.
pub(crate) async fn bind() -> std::io::Result<Arc<UdpSocket>> {
    Ok(Arc::new(UdpSocket::bind("0.0.0.0:0").await?))
}

/// How many datagrams one pass hands the kernel at once.
const SEND_RUN: usize = 16;

/// Put one pass's datagrams on the wire.
///
/// The state machine answers one packet per call, so the pass collects a run and
/// hands it over whole where the platform can segment it: one syscall for the
/// run, and the NIC cuts it, which is the amortisation TCP gets from TSO without
/// leaving the socket API.
#[cfg(target_os = "linux")]
async fn send_run(
    socket: &UdpSocket,
    peer: SocketAddr,
    run: &[u8],
    spans: &[(usize, usize)],
) -> std::io::Result<()> {
    use crate::transport::dgram::Span;
    use crate::transport::udp_batch::SendBatch;
    use bytes::Bytes;
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;

    let staged: Vec<Span> = spans
        .iter()
        .map(|(off, len)| Span::Staged {
            off: *off,
            len: *len,
        })
        .collect();
    let buf = Bytes::copy_from_slice(run);
    socket.writable().await?;
    socket.try_io(Interest::WRITABLE, || {
        // A run GSO cannot take, or a kernel without it, falls back to the
        // batched syscall for the same datagrams.
        match SendBatch::send_gso(socket.as_raw_fd(), peer, &buf, &staged) {
            Ok(0) | Err(_) => SendBatch::send_spans(socket.as_raw_fd(), peer, &buf, &staged),
            Ok(n) => Ok(n),
        }
    })?;
    Ok(())
}

/// The same, one datagram at a time (see the Linux version for the batched one).
#[cfg(not(target_os = "linux"))]
async fn send_run(
    socket: &UdpSocket,
    peer: SocketAddr,
    run: &[u8],
    spans: &[(usize, usize)],
) -> std::io::Result<()> {
    for (off, len) in spans {
        socket.send_to(&run[*off..*off + *len], peer).await?;
    }
    Ok(())
}

/// Drain what the connection wants to say, in as few syscalls as the platform
/// allows.
async fn flush(socket: &UdpSocket, peer: SocketAddr, shared: &Arc<Shared>) -> std::io::Result<()> {
    let mut run: Vec<u8> = Vec::with_capacity(SEND_RUN * 2048);
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(SEND_RUN);
    let mut packet = [0u8; 2048];
    loop {
        run.clear();
        spans.clear();
        while spans.len() < SEND_RUN {
            let now = Instant::now();
            let Some(len) = shared.with(|conn| conn.poll_transmit(now, &mut packet)) else {
                break;
            };
            let off = run.len();
            run.extend_from_slice(&packet[..len]);
            spans.push((off, len));
        }
        if spans.is_empty() {
            return Ok(());
        }
        send_run(socket, peer, &run, &spans).await?;
    }
}

/// The receive side: one batched read, split back into the datagrams it
/// carried.
#[cfg(target_os = "linux")]
async fn receive(
    socket: &UdpSocket,
    batch: &mut crate::transport::udp_batch::RecvBatch,
    mut on_datagram: impl FnMut(std::net::SocketAddr, &[u8]),
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;
    socket.readable().await?;
    match socket.try_io(Interest::READABLE, || batch.recv(socket.as_raw_fd())) {
        Ok(_) => {
            for (from, data) in batch.iter() {
                on_datagram(from, data);
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(e) => Err(e),
    }
}

/// The same, one datagram at a time.
#[cfg(not(target_os = "linux"))]
async fn receive(
    socket: &UdpSocket,
    _batch: &mut (),
    mut on_datagram: impl FnMut(std::net::SocketAddr, &[u8]),
) -> std::io::Result<()> {
    let mut buf = [0u8; 65536];
    let (len, from) = socket.recv_from(&mut buf).await?;
    on_datagram(from, &buf[..len]);
    Ok(())
}

/// Start the pump for one connection that owns its socket.
pub(crate) fn spawn(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    shared: Arc<Shared>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(target_os = "linux")]
        let mut batch = crate::transport::udp_batch::RecvBatch::new(SEND_RUN, 2048);
        #[cfg(not(target_os = "linux"))]
        let mut batch = ();
        loop {
            if flush(&socket, peer, &shared).await.is_err() {
                return;
            }
            let arriving = Arc::clone(&shared);
            tokio::select! {
                result = receive(&socket, &mut batch, |from, data| {
                    if from != peer {
                        return;
                    }
                    let now = Instant::now();
                    arriving.with(|conn| {
                        let _ = conn.on_datagram(now, data);
                    });
                    arriving.wake_readers();
                    arriving.wake_writers();
                }) => {
                    if result.is_err() {
                        return;
                    }
                }
                () = shared.pump_signal() => {}
            }
        }
    })
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
        loop {
            if flush(&socket, peer, &shared).await.is_err() {
                return;
            }
            shared.pump_signal().await;
            if shared.with(|conn| conn.closed().is_some()) {
                return;
            }
        }
    })
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

    /// Wait for a stream the peer opens.
    ///
    /// The dispatcher signals the connection when a datagram opens a stream, so
    /// this parks on that signal rather than polling the queue.
    pub(crate) async fn wait_stream(&self) -> StreamIo {
        loop {
            if let Some(stream) = self.accept_stream() {
                return stream;
            }
            self.shared.pump_signal().await;
        }
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

/// The listening end: one socket, one connection per id it has never seen, and
/// one dispatcher that routes.
#[derive(Debug)]
pub(crate) struct MhdListener {
    addr: SocketAddr,
    incoming: tokio::sync::mpsc::UnboundedReceiver<Arc<MhdLane>>,
}

/// How many listening sockets one acceptor binds.
fn listener_socket_count() -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, 4)
}

/// One non-blocking UDP socket bound with `SO_REUSEPORT`, so the kernel can spread
/// arriving 4-tuples over several sockets instead of one.
fn shared_socket(addr: SocketAddr) -> std::io::Result<UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket)
}

/// Bind the sockets this listener dispatches from: the first takes the port, the
/// rest share it. Without `SO_REUSEPORT` the address cannot be bound twice and the
/// fallback is one socket, which is what the carrier had before.
fn bind_listener_sockets(addr: SocketAddr) -> std::io::Result<Vec<Arc<UdpSocket>>> {
    let first = shared_socket(addr)?;
    let bound = first.local_addr()?;
    let want = listener_socket_count();
    let mut sockets = vec![Arc::new(first)];
    for _ in 1..want {
        match shared_socket(bound) {
            Ok(socket) => sockets.push(Arc::new(socket)),
            Err(_) => break,
        }
    }
    Ok(sockets)
}

/// One socket's dispatcher: read a run, route what it carried.
fn spawn_dispatcher(
    socket: Arc<UdpSocket>,
    registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>>,
    tx: tokio::sync::mpsc::UnboundedSender<Arc<MhdLane>>,
    next_id: Arc<std::sync::atomic::AtomicUsize>,
) {
    #[cfg(target_os = "linux")]
    let mut batch = crate::transport::udp_batch::RecvBatch::new(16, 2048);
    #[cfg(not(target_os = "linux"))]
    let mut batch = ();
    tokio::spawn(async move {
        loop {
            let mut arriving: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
            let read = receive(&socket, &mut batch, |from, data| {
                arriving.push((from, data.to_vec()));
            })
            .await;
            if read.is_err() {
                return;
            }
            for (from, datagram) in arriving {
                let buf = &datagram[..];
                let Ok(packet) = crate::transport::mhd::wire::decode(buf) else {
                    continue;
                };
                let now = Instant::now();
                if let Some(shared) = registry.lock().get(&packet.id).cloned() {
                    shared.with(|conn| {
                        let _ = conn.on_datagram(now, buf);
                    });
                    shared.wake_readers();
                    shared.wake_writers();
                    shared.wake_pump();
                    continue;
                }
                let shared = Shared::new(crate::transport::mhd::conn::Conn::new(packet.id, false));
                shared.with(|conn| {
                    let _ = conn.on_datagram(now, buf);
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
        }
    });
}
impl MhdListener {
    /// Bind the listener and start one dispatcher per socket.
    pub(crate) fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let sockets = bind_listener_sockets(addr)?;
        let bound = sockets[0].local_addr()?;
        let (tx, incoming) = tokio::sync::mpsc::unbounded_channel();
        let registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>> =
            Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new()));
        let next_id = Arc::new(std::sync::atomic::AtomicUsize::new(1));
        for socket in sockets {
            spawn_dispatcher(
                socket,
                Arc::clone(&registry),
                tx.clone(),
                Arc::clone(&next_id),
            );
        }
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
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The pool's own view: a dialing lane and a listening lane, one stream
    /// opened through the trait the pool uses, read on the other side.
    #[tokio::test]
    async fn a_lane_opens_a_stream_the_listener_accepts() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
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
        let waiting = tokio::time::timeout(Duration::from_secs(5), accepted.wait_stream()).await;
        let Ok(mut inbound) = waiting else {
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
