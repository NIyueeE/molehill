//! The socket pump: one task per connection that moves datagrams between the
//! state machine and the UDP socket.
//!
//! It is deliberately dumb. Every pass first drains whatever the connection
//! wants to send, then waits for input; the wait has a deadline, because the
//! connection has timers of its own (a re-send, a keepalive, a dead path) and a
//! pump that only woke on a socket would never fire them. A readiness-driven
//! pump belongs with the pool attachment, where the wake-up path is designed
//! once for both.
//!
//! Two shapes exist: a dialing lane owns its socket and both directions here;
//! an accepted lane's socket is shared by the listener's dispatcher, which
//! routes arriving datagrams by connection id, so its pump only sends.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UdpSocket;

use super::conn::Conn;
use super::lane::{Shared, StreamIo};
use super::recovery::Expiry;
use super::wire::ResetCode;

/// Bind the local socket of a connection.
pub(crate) async fn bind() -> std::io::Result<Arc<UdpSocket>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    tune_socket(&socket);
    Ok(Arc::new(socket))
}

/// The kernel buffer a lane asks for: 32 MiB absorbs the largest burst of a
/// wide-area window, and the request is clamped by the system's
/// `rmem_max`/`wmem_max` anyway. Failures are non-fatal and quiet: the window
/// still bounds what can be in flight, and a debug line is enough for a host
/// that would rather keep its default.
fn tune_socket(socket: &UdpSocket) {
    let s = socket2::SockRef::from(socket);
    if let Err(e) = s.set_recv_buffer_size(32 * 1024 * 1024) {
        tracing::debug!("mhd socket kept its default receive buffer: {e}");
    }
    if let Err(e) = s.set_send_buffer_size(32 * 1024 * 1024) {
        tracing::debug!("mhd socket kept its default send buffer: {e}");
    }
    #[cfg(target_os = "linux")]
    if let Err(e) =
        nix::sys::socket::setsockopt(socket, nix::sys::socket::sockopt::UdpGroSegment, &true)
    {
        // Growable receive is an optimisation, not a requirement: a socket
        // that refuses it keeps one datagram per recvmsg.
        tracing::debug!("mhd socket refused GRO: {e}");
    }
}

/// How many datagrams one pass hands the kernel at once.
const SEND_RUN: usize = 16;

/// How many connections a listener may hold before it stops admitting new
/// ones.
///
/// The carrier has no handshake, so a datagram naming an unknown id allocates
/// state. An off-path flood must not grow the table without bound: the cap is
/// generous for a real deployment and the refusal is silent (the sender of a
/// bogus id learns nothing) except for one debug line.
const MAX_CONNECTIONS: usize = 4096;

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

    let all: Vec<Span> = spans
        .iter()
        .map(|(off, len)| Span::Staged {
            off: *off,
            len: *len,
        })
        .collect();
    let buf = Bytes::copy_from_slice(run);
    let mut at = 0;
    while at < all.len() {
        socket.writable().await?;
        let chunk = &all[at..];
        let outcome = socket.try_io(Interest::WRITABLE, || {
            // A run GSO cannot take, or a kernel without it, falls back to the
            // batched syscall for the same datagrams.
            match SendBatch::send_gso(socket.as_raw_fd(), peer, &buf, chunk) {
                Ok(0) | Err(_) => SendBatch::send_spans(socket.as_raw_fd(), peer, &buf, chunk),
                Ok(n) => Ok(n),
            }
        });
        match outcome {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "the socket accepted none of the datagrams",
                ));
            }
            // A partial `sendmmsg` is normal under memory pressure: the rest of
            // the run goes in the next pass, never as a silent loss.
            Ok(sent) => at += sent,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
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
///
/// One run per call on purpose: a flush that emptied a multi-megabyte send
/// buffer before reading its socket let acknowledgements pile up behind the
/// sends, and the first ack read afterwards jumped hundreds of packet numbers —
/// every one of them then classified lost, and the connection re-sent megabytes
/// the peer already had. The caller alternates a run with a non-blocking ack
/// drain, so send and acknowledgement progress together.
async fn flush(
    socket: &UdpSocket,
    peer: SocketAddr,
    shared: &Arc<Shared>,
) -> std::io::Result<usize> {
    let mut run: Vec<u8> = Vec::with_capacity(SEND_RUN * 2048);
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(SEND_RUN);
    let mut packet = [0u8; 2048];
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
        return Ok(0);
    }
    let sent = spans.len();
    send_run(socket, peer, &run, &spans).await?;
    for _ in 0..sent {
        shared.note_sent();
    }
    Ok(sent)
}

/// Read whatever the socket has right now, without waiting for readiness.
///
/// The dialing pump alternates this with its sends, so an acknowledgement is
/// applied before the next run is built. A server-side lane does not use it:
/// its socket's receive side belongs to the listener's dispatcher.
#[cfg(target_os = "linux")]
fn drain_ready(
    socket: &UdpSocket,
    batch: &mut crate::transport::udp_batch::RecvBatch,
    shared: &Arc<Shared>,
    peer: SocketAddr,
) {
    use std::os::fd::AsRawFd;
    use tokio::io::Interest;
    let outcome = socket.try_io(Interest::READABLE, || batch.recv(socket.as_raw_fd()));
    match outcome {
        Ok(_) => {
            let now = Instant::now();
            for (from, data) in batch.iter() {
                if from != peer {
                    continue;
                }
                shared.with(|conn| {
                    let _ = conn.on_datagram(now, data);
                });
            }
            shared.wake_readers();
            shared.wake_writers();
            shared.wake_streams();
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
        Err(_) => {}
    }
}

/// The same, one datagram at a time.
#[cfg(not(target_os = "linux"))]
fn drain_ready(socket: &UdpSocket, _batch: &mut (), shared: &Arc<Shared>, peer: SocketAddr) {
    loop {
        let mut buf = [0u8; 65536];
        match socket.try_recv_from(&mut buf) {
            Ok((len, from)) => {
                if from != peer {
                    continue;
                }
                let now = Instant::now();
                shared.with(|conn| {
                    let _ = conn.on_datagram(now, &buf[..len]);
                });
                shared.wake_readers();
                shared.wake_writers();
                shared.wake_streams();
            }
            Err(_) => return,
        }
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

/// Why the pump's wait ended.
enum Wake {
    /// Something new is queued: go round and flush it.
    Signal,
    /// The connection's timer fired: run its expiry.
    Timer,
    /// The connection ended.
    Closed,
}

/// Wait for whichever of the three reasons to look again comes first.
async fn wait(shared: &Arc<Shared>) -> Wake {
    let deadline = shared.with(|conn| conn.poll_timeout());
    let timer = async {
        match deadline {
            Some(at) => tokio::time::sleep_until(tokio::time::Instant::from_std(at)).await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        () = shared.pump_signal() => Wake::Signal,
        () = shared.closed() => Wake::Closed,
        () = timer => Wake::Timer,
    }
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
            let Ok(sent) = flush(&socket, peer, &shared).await else {
                shared.close(ResetCode::Aborted);
                return;
            };
            if sent > 0 {
                drain_ready(&socket, &mut batch, &shared, peer);
            }
            if shared.is_closed() {
                shared.finish_close();
                return;
            }
            if sent == SEND_RUN {
                // More is queued: send it, but only after the drain above had
                // its chance to apply the peer's acknowledgements, and after a
                // yield so a peer task (the server's dispatcher) can run.
                tokio::task::yield_now().await;
                continue;
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
                    arriving.wake_streams();
                }) => {
                    if result.is_err() {
                        shared.close(ResetCode::Aborted);
                        return;
                    }
                }
                keep = wait(&shared) => {
                    match keep {
                        Wake::Signal => {}
                        Wake::Closed => {
                            // One last pass so a queued reset or FIN leaves
                            // before the socket does; a failed send ends the
                            // lane anyway.
                            let _ = flush(&socket, peer, &shared).await;
                            shared.finish_close();
                            return;
                        }
                        Wake::Timer => {
                            let now = Instant::now();
                            let (expiry, _) = shared.with(|conn| conn.on_timeout(now));
                            if expiry == Expiry::Dead {
                                shared.close(ResetCode::Aborted);
                            }
                            shared.wake_readers();
                            shared.wake_writers();
                        }
                    }
                }
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
            let Ok(sent) = flush(&socket, peer, &shared).await else {
                shared.close(ResetCode::Aborted);
                return;
            };
            if sent == SEND_RUN {
                // More is queued; the listener's dispatcher keeps the
                // acknowledgements flowing while this pass sends, but give the
                // runtime a chance to run it.
                tokio::task::yield_now().await;
                continue;
            }
            match wait(&shared).await {
                Wake::Signal => {}
                Wake::Closed => {
                    let _ = flush(&socket, peer, &shared).await;
                    shared.finish_close();
                    return;
                }
                Wake::Timer => {
                    let now = Instant::now();
                    let (expiry, _) = shared.with(|conn| conn.on_timeout(now));
                    if expiry == Expiry::Dead {
                        shared.close(ResetCode::Aborted);
                    }
                    shared.wake_readers();
                    shared.wake_writers();
                }
            }
        }
    })
}

/// One connection as the pool holds it: a shared state machine, the counters
/// its streams are charged to, and the socket its pump sends on.
pub(crate) struct MhdLane {
    id: usize,
    shared: Arc<Shared>,
    counters: Arc<crate::transport::multiplex::TunnelCounters>,
    /// The Noise keys a stream opened on this lane is wrapped in, when the
    /// service's data plane is encrypted. Per stream, not per lane: every
    /// application stream is its own record stream, so no stream can read
    /// another's bytes and a reordering never desynchronises a counter.
    noise: Option<crate::config::NoiseConfig>,
    /// The session nonce every stream's hello names.
    nonce: crate::protocol::Digest,
}

impl std::fmt::Debug for MhdLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MhdLane")
            .field("id", &self.id)
            .field("alive", &(!self.shared.is_closed()))
            .finish_non_exhaustive()
    }
}

/// Dropping the lane ends its connection once what is queued has left.
///
/// The pump task holds only the shared state (and the socket), so without this
/// a removed lane would leave a live socket and a task behind for ever; the
/// graceful close lets a queued reset reach the peer first.
impl Drop for MhdLane {
    fn drop(&mut self) {
        self.shared.begin_close(ResetCode::Aborted);
    }
}

impl MhdLane {
    /// The lane's process-unique id.
    pub(crate) fn id(&self) -> usize {
        self.id
    }

    /// Dial a connection to a listening endpoint, without proving it accepted.
    ///
    /// A direct data channel uses this: its own hello is the proof, and paying
    /// a throwaway stream for a second one would add a round trip to every
    /// visitor.
    pub(crate) async fn dial(
        peer: SocketAddr,
        id: usize,
        noise: Option<crate::config::NoiseConfig>,
        nonce: crate::protocol::Digest,
    ) -> std::io::Result<Arc<Self>> {
        let socket = bind().await?;
        let shared = Shared::new(
            Conn::new(u32::try_from(id).unwrap_or(1), true),
            crate::transport::health::SessionHealth::new(),
        );
        let task = spawn(Arc::clone(&socket), peer, Arc::clone(&shared));
        // The task owns the socket and the lane; when the lane drops, the
        // connection closes and the task's next pass ends it.
        std::mem::forget(task);
        Ok(Arc::new(Self {
            id,
            counters: crate::transport::multiplex::TunnelCounters::for_lane_with(Some(Arc::clone(
                &shared.health,
            ))),
            shared,
            noise,
            nonce,
        }))
    }

    /// Dial a session and prove the peer accepted it.
    ///
    /// The proof is one stream that completes the tunnel hello and is then let
    /// go: the server reserves the lane's slot on that hello, so a pool never
    /// holds a lane the valve refused.
    pub(crate) async fn connect(
        peer: SocketAddr,
        id: usize,
        noise: Option<crate::config::NoiseConfig>,
        nonce: crate::protocol::Digest,
    ) -> anyhow::Result<Arc<Self>> {
        use tokio::io::AsyncWriteExt;
        let lane = Self::dial(peer, id, noise, nonce).await?;
        let mut stream = crate::transport::multiplex::PoolLane::open_stream(&*lane)
            .await
            .map_err(|e| anyhow::anyhow!("the mhd lane refused its opening stream: {e}"))?;
        stream.shutdown().await?;
        Ok(lane)
    }

    /// Open one application stream: the transport selector, the optional Noise
    /// handshake, and nothing above that.
    ///
    /// The caller owns what the stream means (a tunnel hello, a direct data
    /// channel's hello); the lane owns the carrier's part of the opening.
    pub(crate) async fn open_channel(
        &self,
    ) -> Result<crate::transport::multiplex::CarrierStream, crate::transport::multiplex::LaneError>
    {
        use tokio::io::AsyncWriteExt;
        if self.shared.is_closed() {
            return Err(crate::transport::multiplex::LaneError::Closed);
        }
        let id = self.shared.with(|conn| conn.open().id);
        self.shared.wake_pump();
        let plain: crate::transport::multiplex::CarrierStream =
            Box::new(StreamIo::new(Arc::clone(&self.shared), id));
        match &self.noise {
            #[cfg(feature = "noise")]
            Some(config) => {
                let keys = crate::transport::NoiseKeys::from_config(config)
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                let wrapped = keys
                    .wrap_initiator(plain)
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                Ok(Box::new(wrapped))
            }
            // Without the `noise` feature `noise` is always None, so this arm
            // is the only one the compiler needs there.
            _ => {
                let mut plain = plain;
                plain
                    .write_all(&[crate::protocol::PLAIN_SELECTOR])
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                plain
                    .flush()
                    .await
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
                Ok(plain)
            }
        }
    }

    /// Wait for a stream the peer opens.
    ///
    /// The dispatcher signals the connection when a datagram opens a stream, so
    /// this parks on that signal rather than polling the queue. A connection
    /// that ends wakes it with `None`.
    pub(crate) async fn wait_stream(&self) -> Option<StreamIo> {
        loop {
            if let Some(stream) = self.accept_stream() {
                return Some(stream);
            }
            tokio::select! {
                () = self.shared.closed() => return None,
                () = self.shared.stream_added() => {}
            }
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
        !self.shared.is_closed()
    }

    fn mark_dead(&self) {
        self.shared.begin_close(ResetCode::Aborted);
    }

    async fn open_stream(
        &self,
    ) -> Result<crate::transport::multiplex::CarrierStream, crate::transport::multiplex::LaneError>
    {
        use tokio::io::AsyncWriteExt;
        let mut stream = self.open_channel().await?;
        let hello = crate::protocol::Hello::DataChannelTunnelHello(
            crate::protocol::CURRENT_PROTO_VERSION,
            self.nonce,
        );
        stream
            .write_all(
                &postcard::to_stdvec(&hello)
                    .map_err(|_| crate::transport::multiplex::LaneError::Closed)?,
            )
            .await
            .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
        stream
            .flush()
            .await
            .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
        let ack = crate::protocol::read_ack(&mut stream)
            .await
            .map_err(|_| crate::transport::multiplex::LaneError::Closed)?;
        match ack {
            crate::protocol::Ack::Ok => {
                tracing::debug!(lane = self.id, "mhd stream opened and acknowledged");
                Ok(stream)
            }
            _ => Err(crate::transport::multiplex::LaneError::Closed),
        }
    }
}

/// The listener's cap on live connections: see [`MAX_CONNECTIONS`].
///
/// A refused datagram is dropped without a reply: the carrier must not become
/// an amplifier, and the peer that guesses no id learns nothing.
static CONNECTION_TABLE_FULL: crate::logging::RepeatNotice = crate::logging::RepeatNotice::new();

fn admit(registry: &mut std::collections::HashMap<u32, Arc<Shared>>, id: u32) -> bool {
    if registry.len() < MAX_CONNECTIONS {
        return true;
    }
    CONNECTION_TABLE_FULL.report(
        || {
            tracing::warn!(
                "mhd listener refused a new connection: its table holds {MAX_CONNECTIONS} and \
                 the cap is full. Further refusals are logged at debug level."
            );
        },
        || tracing::debug!("mhd listener refused connection {id}: table full"),
    );
    false
}

/// The listening end: one socket per core, one connection per id it has never
/// seen, and one dispatcher per socket that routes.
pub(crate) struct MhdListener {
    addr: SocketAddr,
    incoming: tokio::sync::mpsc::UnboundedReceiver<Arc<MhdLane>>,
    /// Every connection the listener admitted, so shutdown can end them.
    ///
    /// An accepted lane's sender pump holds a clone of a *listening* socket;
    /// leaving one alive after the listener stops keeps the port bound (and,
    /// under `SO_REUSEPORT`, lets a restarted server's traffic land on the
    /// dead one). Closing each connection releases its socket.
    registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>>,
    /// Held so a dropped listener closes its sockets: the dispatcher tasks
    /// each hold one socket, and a listener nobody polls has no other way to
    /// release the port.
    _sockets: Vec<Arc<UdpSocket>>,
    /// Stops the dispatchers when the listener goes away.
    stop: tokio::sync::watch::Sender<bool>,
    _next_id: Arc<std::sync::atomic::AtomicUsize>,
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
    let socket = UdpSocket::from_std(std_socket)?;
    tune_socket(&socket);
    Ok(socket)
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
///
/// It stops on `stop`, which is what lets a restarted server bind the port
/// alone: a dispatcher that outlived its listener would keep a `SO_REUSEPORT`
/// socket bound and take a share of the next server's datagrams.
fn spawn_dispatcher(
    socket: Arc<UdpSocket>,
    registry: Arc<parking_lot::Mutex<std::collections::HashMap<u32, Arc<Shared>>>>,
    tx: tokio::sync::mpsc::UnboundedSender<Arc<MhdLane>>,
    next_id: Arc<std::sync::atomic::AtomicUsize>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    #[cfg(target_os = "linux")]
    let mut batch = crate::transport::udp_batch::RecvBatch::new(16, 2048);
    #[cfg(not(target_os = "linux"))]
    let mut batch = ();
    tokio::spawn(async move {
        loop {
            let mut arriving: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
            let read = tokio::select! {
                changed = stop.changed() => {
                    let _ = changed;
                    return;
                }
                read = receive(&socket, &mut batch, |from, data| {
                    arriving.push((from, data.to_vec()));
                }) => read,
            };
            if read.is_err() {
                return;
            }
            for (from, datagram) in arriving {
                let buf = &datagram[..];
                let Ok(packet) = crate::transport::mhd::wire::decode(buf) else {
                    continue;
                };
                let now = Instant::now();
                {
                    let existing = registry.lock().get(&packet.id).cloned();
                    if let Some(shared) = existing {
                        shared.with(|conn| {
                            let _ = conn.on_datagram(now, buf);
                        });
                        shared.wake_readers();
                        shared.wake_writers();
                        shared.wake_pump();
                        shared.wake_streams();
                        continue;
                    }
                }
                {
                    let mut table = registry.lock();
                    if !admit(&mut table, packet.id) {
                        continue;
                    }
                    let shared = Shared::new(
                        Conn::new(packet.id, false),
                        crate::transport::health::SessionHealth::new(),
                    );
                    shared.with(|conn| {
                        let _ = conn.on_datagram(now, buf);
                    });
                    table.insert(packet.id, Arc::clone(&shared));
                    drop(table);
                    let id = next_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let task = spawn_sender(Arc::clone(&socket), from, Arc::clone(&shared));
                    std::mem::forget(task);
                    let lane = Arc::new(MhdLane {
                        id,
                        counters: crate::transport::multiplex::TunnelCounters::for_lane_with(Some(
                            Arc::clone(&shared.health),
                        )),
                        shared,
                        noise: None,
                        nonce: [0u8; crate::protocol::HASH_WIDTH_IN_BYTES],
                    });
                    if tx.send(lane).is_err() {
                        return;
                    }
                }
            }
            // A socket that always has data would keep this task ready on every
            // await and starve the connection pumps, whose acknowledgements are
            // what let the senders forget the data. Yield once per batch so an
            // ack goes out well inside the window it describes.
            tokio::task::yield_now().await;
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
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        for socket in &sockets {
            spawn_dispatcher(
                Arc::clone(socket),
                Arc::clone(&registry),
                tx.clone(),
                Arc::clone(&next_id),
                stop_rx.clone(),
            );
        }
        Ok(Self {
            addr: bound,
            incoming,
            registry,
            _sockets: sockets,
            stop,
            _next_id: next_id,
        })
    }

    /// The address it bound.
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// End every connection this listener admitted and release its sockets.
    ///
    /// Called when the listener stops serving: a lane that outlived shutdown
    /// would hold a listening socket bound, and the restarted server would
    /// share the port with a connection table nobody reads. The dispatchers are
    /// stopped with the lanes — leaving one reading would keep a `SO_REUSEPORT`
    /// socket bound and take a share of the next server's datagrams.
    pub(crate) fn close_all(&self) {
        let _ = self.stop.send(true);
        let mut table = self.registry.lock();
        for shared in table.values() {
            shared.close(ResetCode::Aborted);
        }
        table.clear();
    }

    /// The next connection a peer opens.
    ///
    /// A datagram naming an id this end has never seen starts the connection:
    /// there is no handshake to wait for, which is the point of the carrier.
    pub(crate) async fn accept(&mut self) -> Option<Arc<MhdLane>> {
        self.incoming.recv().await
    }
}

/// A listener that is dropped without an explicit shutdown still releases its
/// port: the dispatchers are told to stop, and the sockets follow when they do.
impl Drop for MhdListener {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
    }
}

#[cfg(test)]
mod lane_tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    #![expect(
        clippy::panic,
        reason = "a stalled round trip is exactly what these tests exist to \
                  catch, and a panic names the step that stalled"
    )]
    use super::*;
    use crate::transport::multiplex::PoolLane;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// The transport seam: a dialing lane opens one stream and a listening lane
    /// reads it back, over real UDP sockets.
    ///
    /// The stream here is a bare channel, not a pool open: a pool open carries
    /// the tunnel hello and waits for the server's ack, which only the server
    /// speaks (the integration suite covers that path).
    #[tokio::test]
    async fn a_lane_opens_a_stream_the_listener_accepts() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 3, None, [7u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();

        let mut stream = dialer.open_channel().await.unwrap();
        tokio::spawn(async move {
            stream.write_all(b"lane to lane").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let waiting = tokio::time::timeout(Duration::from_secs(5), accepted.wait_stream()).await;
        let Some(Some(mut inbound)) = waiting.ok() else {
            unreachable!("the listener must accept the stream")
        };
        let mut got = Vec::new();
        let selector = tokio::time::timeout(Duration::from_secs(5), inbound.read_u8())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(selector, crate::protocol::PLAIN_SELECTOR);
        let read =
            tokio::time::timeout(Duration::from_secs(5), inbound.read_to_end(&mut got)).await;
        let Ok(Ok(_)) = read else {
            unreachable!("the read must finish: got {got:?}")
        };
        assert_eq!(got, b"lane to lane");
        assert!(PoolLane::is_alive(&*accepted));
    }

    /// A stream carries a real conversation: many round trips, each one a
    /// request the peer answers. This is the shape every forwarded TCP channel
    /// has, and it is where a lost wake-up or a stalled credit shows up.
    #[tokio::test]
    async fn a_stream_carries_a_hundred_round_trips() {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 4, None, [9u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();
        let mut client = dialer.open_channel().await.unwrap();

        let opening = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            // The opening byte is the transport selector, which the server
            // consumes before any application data.
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            assert_eq!(selector[0], crate::protocol::PLAIN_SELECTOR);
            let mut got = vec![0u8; 1024];
            loop {
                match server.read_exact(&mut got).await {
                    Ok(_) => {
                        tokio::io::AsyncWriteExt::write_all(&mut server, &got)
                            .await
                            .unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
        });

        let mut wr = [0u8; 1024];
        let mut rd = [0u8; 1024];
        for step in 0u8..100 {
            wr[0] = step;
            let round = async {
                tokio::io::AsyncWriteExt::write_all(&mut client, &wr)
                    .await
                    .unwrap();
                tokio::io::AsyncReadExt::read_exact(&mut client, &mut rd)
                    .await
                    .unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5), round)
                .await
                .unwrap_or_else(|_| panic!("round trip {step} stalled"));
            assert_eq!(wr, rd);
        }
        drop(client);
        opening.await.unwrap();
    }

    /// The same conversation through the Noise record stream: this is the
    /// product's actual stream stack, and the handshake runs inside the stream
    /// open, which is where a partial read or a stalled credit would show.
    #[cfg(feature = "noise")]
    #[tokio::test]
    async fn a_noise_stream_carries_a_hundred_round_trips() {
        use crate::config::{MaskedString, NoiseConfig};

        let client_keys = NoiseConfig {
            public_key: "mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg=".to_owned(),
            private_key: MaskedString::from("8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="),
        };
        let server_keys = NoiseConfig {
            public_key: "zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE=".to_owned(),
            private_key: MaskedString::from("kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE="),
        };

        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let keys = crate::transport::NoiseKeys::from_config(&server_keys).unwrap();
        let opening = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            assert_eq!(selector[0], crate::protocol::NOISE_SELECTOR);
            let server: crate::transport::multiplex::CarrierStream = Box::new(server);
            let mut server = keys.wrap_responder(server).await.unwrap();
            let mut got = vec![0u8; 1024];
            loop {
                match server.read_exact(&mut got).await {
                    Ok(_) => {
                        tokio::io::AsyncWriteExt::write_all(&mut server, &got)
                            .await
                            .unwrap();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
        });

        let dialer = MhdLane::dial(
            bound,
            5,
            Some(client_keys),
            [3u8; crate::protocol::HASH_WIDTH_IN_BYTES],
        )
        .await
        .unwrap();
        let mut client = dialer.open_channel().await.unwrap();
        let mut wr = [0u8; 1024];
        let mut rd = [0u8; 1024];
        for step in 0u8..100 {
            wr[0] = step;
            let round = async {
                tokio::io::AsyncWriteExt::write_all(&mut client, &wr)
                    .await
                    .unwrap();
                tokio::io::AsyncReadExt::read_exact(&mut client, &mut rd)
                    .await
                    .unwrap();
            };
            tokio::time::timeout(Duration::from_secs(5), round)
                .await
                .unwrap_or_else(|_| panic!("noise round trip {step} stalled"));
            assert_eq!(wr, rd);
        }
        drop(client);
        opening.await.unwrap();
    }

    /// A bulk write whose reader is slow: the sender must keep every byte even
    /// when the reader only drains a slice every few milliseconds, exactly the
    /// shape of an echo service feeding a visitor nobody reads yet.
    #[tokio::test]
    async fn a_bulk_stream_survives_a_slow_reader() {
        let total = 8 * 1024 * 1024;
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut listener = MhdListener::bind(addr).unwrap();
        let bound = listener.local_addr();
        let dialer = MhdLane::dial(bound, 6, None, [5u8; crate::protocol::HASH_WIDTH_IN_BYTES])
            .await
            .unwrap();
        let mut client = dialer.open_channel().await.unwrap();

        let reading = tokio::spawn(async move {
            let accepted = listener.accept().await.unwrap();
            let mut server = accepted.wait_stream().await.unwrap();
            let mut selector = [0u8; 1];
            server.read_exact(&mut selector).await.unwrap();
            let mut got = vec![0u8; 64 * 1024];
            let mut read = 0usize;
            loop {
                match server.read(&mut got).await {
                    Ok(0) => break,
                    Ok(n) => {
                        read += n;
                        if read % (512 * 1024) < n {
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    }
                    Err(e) => panic!("server read failed: {e}"),
                }
            }
            read
        });

        let chunk = vec![0xA5u8; 64 * 1024];
        let mut written = 0;
        while written < total {
            client.write_all(&chunk).await.unwrap();
            written += chunk.len();
        }
        client.shutdown().await.unwrap();
        let read = tokio::time::timeout(Duration::from_secs(20), reading)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            read, total,
            "the reader must see every byte the writer sent"
        );
    }
}
