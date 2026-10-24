//! The endpoint task: one socket, one driver, one loop.
//!
//! `quinn-proto` never touches a socket or a timer, so something has to: this
//! task owns the UDP socket, feeds received datagrams into the driver, drains
//! what the driver wants to send, and sleeps exactly until the earliest
//! connection deadline. Commands arrive over a channel and answers leave over
//! one-shot replies, so the carrier's async surface stays small.
//!
//! The batching sender (`udp_batch`, `UDP_SEGMENT`) slots in here later: the
//! driver already reports a GSO segment size, and the only change is which
//! syscall carries the bytes.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Result, anyhow};
use bytes::BytesMut;
use parking_lot::Mutex;
use quinn_proto::{ClientConfig, ConnectionHandle, Dir, Event, ServerConfig, StreamEvent};
use tokio::net::UdpSocket;
use tokio::sync::{Notify, mpsc, oneshot};

use super::conn::{self, QuicStream, Wait};
use super::driver::{Driver, Outgoing};
use super::session::HandshakeData;

/// A command from the carrier's async surface into the task.
enum Command {
    Connect {
        remote: SocketAddr,
        config: ClientConfig,
        reply: oneshot::Sender<Result<ConnectionHandle, String>>,
    },
    OpenStream {
        connection: ConnectionHandle,
        /// Whether the stream carries the record layer, i.e. the service's
        /// `data_enable`.
        encrypt: bool,
        reply: oneshot::Sender<std::io::Result<QuicStream>>,
    },
    /// Read one connection's numbers.
    Stats {
        connection: ConnectionHandle,
        reply: oneshot::Sender<crate::transport::carrier::CarrierStats>,
    },
    /// Wait for a connection's handshake.
    AwaitConnected {
        connection: ConnectionHandle,
        reply: oneshot::Sender<()>,
    },
    /// Connect and open one stream, answering only once the handshake is up:
    /// the shape a direct data channel has.
    ConnectStream {
        remote: SocketAddr,
        config: ClientConfig,
        encrypt: bool,
        reply: oneshot::Sender<std::io::Result<QuicStream>>,
    },
}

/// A connection that still owes its caller a stream.
struct Waiting {
    reply: oneshot::Sender<std::io::Result<QuicStream>>,
    encrypt: bool,
}

/// What the task needs to report upward: a connection the peer opened, and
/// every event a connection raised.
pub(crate) struct Channels {
    pub(crate) accepted: mpsc::UnboundedSender<ConnectionHandle>,
    pub(crate) events: mpsc::UnboundedSender<(ConnectionHandle, Event)>,
    /// Every stream the peer opens, as soon as the engine reports it.
    pub(crate) streams: mpsc::UnboundedSender<QuicStream>,
    /// A connection the engine let go, by the opaque key its streams carry
    /// (`QuicStream::connection_key`): the listener's per-connection state
    /// (a pool tunnel's reservation) is released on this, not on a poll.
    pub(crate) lost: mpsc::UnboundedSender<u64>,
}

/// How many already-queued datagrams one readiness wake may consume before
/// the loop drains once. Bounded so a flood cannot starve commands, timers or
/// the stream tasks the same loop serves.
const RECV_BURST: usize = 128;

/// How many engine transmissions may wait for the sender task. Bounded so a
/// path that cannot drain them cannot grow the endpoint's memory without
/// bound; an overflow is a dropped transmit, which QUIC's loss recovery
/// already knows how to repair.
const SEND_QUEUE: usize = 256;

/// Set once a kernel or path has refused `UDP_SEGMENT`, so every later
/// transmission goes out the per-datagram way without probing again.
#[cfg(target_os = "linux")]
static GSO_DISABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) struct EndpointHandle {
    commands: mpsc::Sender<Command>,
    /// Every connection the task still holds, so a carrier can answer
    /// "is this one alive" without guessing.
    live: Arc<Mutex<HashSet<ConnectionHandle>>>,
    /// The ones whose handshake is done: a stream cannot be opened before
    /// that, so the seam waits on this instead of failing.
    connected: Arc<Mutex<HashSet<ConnectionHandle>>>,
}

impl EndpointHandle {
    /// Start an outgoing connection.
    ///
    /// # Errors
    ///
    /// Fails when the engine refuses the connection (a malformed address, an
    /// exhausted connection-id space) or the task is gone.
    pub(crate) async fn connect(
        &self,
        remote: SocketAddr,
        config: ClientConfig,
    ) -> Result<ConnectionHandle> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Connect {
                remote,
                config,
                reply,
            })
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?;
        answer
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?
            .map_err(|reason| anyhow!(reason))
    }

    /// Open a bidirectional stream on a live connection.
    ///
    /// # Errors
    ///
    /// Fails when the peer's stream limit is exhausted, or the task is gone.
    /// Connect and hand back one stream, once the handshake allows it.
    ///
    /// # Errors
    ///
    /// Fails when the connection cannot be established, the handshake has no
    /// record keys for a stream that asked for them, or the task is gone.
    pub(crate) async fn connect_stream(
        &self,
        remote: SocketAddr,
        config: ClientConfig,
        encrypt: bool,
    ) -> Result<QuicStream> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::ConnectStream {
                remote,
                config,
                encrypt,
                reply,
            })
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?;
        answer
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?
            .map_err(|error| anyhow!(error))
    }

    /// What the engine says about one connection.
    ///
    /// # Errors
    ///
    /// Fails when the task is gone; a connection the task no longer holds
    /// answers with zeroes, which is what a dead tunnel looks like.
    pub(crate) async fn stats(
        &self,
        connection: ConnectionHandle,
    ) -> Result<crate::transport::carrier::CarrierStats> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::Stats { connection, reply })
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?;
        answer
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))
    }

    /// Whether the task still holds this connection.
    pub(crate) fn is_alive(&self, connection: ConnectionHandle) -> bool {
        self.live.lock().contains(&connection)
    }

    /// Wait until the handshake allows a stream on this connection.
    ///
    /// # Errors
    ///
    /// Fails when the connection is gone, or the task is gone.
    pub(crate) async fn await_connected(&self, connection: ConnectionHandle) -> Result<()> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::AwaitConnected { connection, reply })
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?;
        answer
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))
    }

    pub(crate) async fn open_stream(
        &self,
        connection: ConnectionHandle,
        encrypt: bool,
    ) -> Result<QuicStream> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::OpenStream {
                connection,
                encrypt,
                reply,
            })
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?;
        answer
            .await
            .map_err(|_| anyhow!("the endpoint task is gone"))?
            .map_err(|error| anyhow!(error))
    }
}

/// What the kernel may hold for one carrier socket, in each direction.
///
/// The kernel default is 212 KiB, under two milliseconds of a gigabit path, so
/// one scheduling hiccup on the receiving side becomes a burst of drops the
/// engine reads as congestion. The bench's `loss1` cell showed the cost: a
/// lane declared a third of everything it sent lost while its peer's queue was
/// full, and the connection never recovered. KCP asks for the same thing on
/// its socket (`KCP_SOCKET_BUF_BYTES`); a QUIC endpoint needs it for the same
/// reason, and this is the one place every endpoint's socket passes through.
/// It is the same 32 MiB KCP asks for: at 5 Gbit/s a burst the receive task is
/// busy for 10 ms is 6 MB, and anything smaller turns a scheduling hiccup into
/// a loss event the engine reads as congestion.
const SOCKET_BUF_BYTES: usize = 32 * 1024 * 1024;

/// Ask the kernel for a carrier-sized socket buffer.
///
/// Best effort: `net.core.{r,w}mem_max` caps the request, and a socket that
/// keeps its default still works — it just recovers less gracefully under a
/// burst. A refusal is not the connection's problem, so it is a debug line
/// rather than a warning.
fn tune_socket_buffers(socket: &UdpSocket) {
    let sock = socket2::SockRef::from(socket);
    if let Err(error) = sock.set_recv_buffer_size(SOCKET_BUF_BYTES) {
        tracing::debug!(%error, "the kernel refused a larger receive buffer");
    }
    if let Err(error) = sock.set_send_buffer_size(SOCKET_BUF_BYTES) {
        tracing::debug!(%error, "the kernel refused a larger send buffer");
    }
}

/// Spawn the task and return the handle the carrier talks to.
pub(crate) fn spawn(
    socket: UdpSocket,
    server_config: Option<Arc<ServerConfig>>,
    allow_mtud: bool,
    channels: Channels,
) -> (EndpointHandle, tokio::task::JoinHandle<()>) {
    tune_socket_buffers(&socket);
    let (commands, inbox) = mpsc::channel(16);
    let live = Arc::new(Mutex::new(HashSet::new()));
    let connected = Arc::new(Mutex::new(HashSet::new()));
    let handle = EndpointHandle {
        commands,
        live: Arc::clone(&live),
        connected: Arc::clone(&connected),
    };
    let task = tokio::spawn(run(
        socket,
        server_config,
        allow_mtud,
        channels,
        inbox,
        live,
        connected,
    ));
    (handle, task)
}

/// Feed one received datagram to the engine, accepting the connection it may
/// have opened. A refused connection is the peer's problem, not this task's:
/// it is logged at debug and the socket keeps receiving.
fn feed_datagram(
    driver: &mut Driver,
    channels: &Channels,
    live: &Arc<Mutex<HashSet<ConnectionHandle>>>,
    remote: SocketAddr,
    data: BytesMut,
) {
    if let Some(incoming) = driver.on_datagram(Instant::now(), remote, data) {
        match driver.accept(Instant::now(), incoming) {
            Ok(handle) => {
                live.lock().insert(handle);
                let _ = channels.accepted.send(handle);
            }
            Err(error) => {
                tracing::debug!(?error, "the engine refused an incoming connection");
            }
        }
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the endpoint task is one select/drain state machine: every select arm has to leave the \
              same loop state for the drain below, and splitting it hides that invariant"
)]
async fn run(
    socket: UdpSocket,
    server_config: Option<Arc<ServerConfig>>,
    allow_mtud: bool,
    channels: Channels,
    mut commands: mpsc::Receiver<Command>,
    live: Arc<Mutex<HashSet<ConnectionHandle>>>,
    connected: Arc<Mutex<HashSet<ConnectionHandle>>>,
) {
    let notify = Arc::new(Notify::new());
    let (recycled, recycled_rx) = mpsc::channel::<Vec<u8>>(SEND_QUEUE);
    let mut driver = Driver::new(server_config, allow_mtud, Arc::clone(&notify), recycled_rx);
    let mut waiting: HashMap<ConnectionHandle, Waiting> = HashMap::new();
    let mut waiters: HashMap<ConnectionHandle, Vec<oneshot::Sender<()>>> = HashMap::new();
    let mut buf = vec![0u8; 64 * 1024];

    // Sends live in their own task. A `send_to` that waits for a full qdisc
    // must not stall the receive path: under loss the two would deadlock into
    // a PTO spiral (measured: a 774 ms read gap and ~1.6 Mbit/s on a 1 % loss
    // path). The queue is bounded; a transmit that does not fit is dropped and
    // the engine's own loss recovery re-emits it, exactly as if the path had
    // dropped it.
    let socket = Arc::new(socket);
    let (send, mut sends) = mpsc::channel::<Outgoing>(SEND_QUEUE);
    let sender_socket = Arc::clone(&socket);
    tokio::spawn(async move {
        while let Some(outgoing) = sends.recv().await {
            if let Err(error) = send_outgoing(&sender_socket, &outgoing).await {
                tracing::debug!(%error, "the socket could not send");
            }
            // Hand the buffer back so the engine writes into it again: the
            // send path then copies nothing at all.
            let _ = recycled.send(outgoing.contents).await;
        }
    });
    loop {
        let now = Instant::now();
        let deadline = driver.poll_timeout();
        let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(
            deadline.unwrap_or_else(|| now + std::time::Duration::from_secs(60)),
        ));
        tokio::pin!(sleep);

        tokio::select! {
            command = commands.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::Connect { remote, config, reply } => {
                        let outcome = driver
                            .connect(Instant::now(), config, remote)
                            .map_err(|error| format!("{error}"));
                        if let Ok(handle) = &outcome {
                            live.lock().insert(*handle);
                        }
                        let _ = reply.send(outcome);
                    }
                    Command::Stats { connection, reply } => {
                        let stats = match driver.shared(connection) {
                            Some(shared) => {
                                let mut connection = shared.connection.lock();
                                let engine = connection.stats();
                                let streams = connection.streams().send_streams();
                                crate::transport::carrier::CarrierStats {
                                    streams,
                                    rtt: Some(engine.path.rtt),
                                    lost_packets: engine.path.lost_packets,
                                }
                            }
                            None => crate::transport::carrier::CarrierStats::default(),
                        };
                        let _ = reply.send(stats);
                    }
                    Command::AwaitConnected { connection, reply } => {
                        if connected.lock().contains(&connection) {
                            let _ = reply.send(());
                        } else if live.lock().contains(&connection) {
                            waiters.entry(connection).or_default().push(reply);
                        } else {
                            // Gone: let the caller's next call report it.
                            let _ = reply.send(());
                        }
                    }
                    Command::ConnectStream {
                        remote,
                        config,
                        encrypt,
                        reply,
                    } => match driver.connect(Instant::now(), config, remote) {
                        Ok(handle) => {
                            live.lock().insert(handle);
                            waiting.insert(handle, Waiting { reply, encrypt });
                        }
                        Err(error) => {
                            let _ = reply.send(Err(std::io::Error::other(format!("{error}"))));
                        }
                    },
                    Command::OpenStream {
                        connection,
                        encrypt,
                        reply,
                    } => {
                        let outcome = match driver.shared(connection) {
                            Some(shared) => conn::open(&shared, encrypt),
                            None => Err(std::io::Error::new(
                                std::io::ErrorKind::NotConnected,
                                "the connection is gone",
                            )),
                        };
                        let _ = reply.send(outcome);
                    }
                }
            }
            received = socket.recv_from(&mut buf) => {
                match received {
                    Ok((len, remote)) => {
                        feed_datagram(
                            &mut driver,
                            &channels,
                            &live,
                            remote,
                            BytesMut::from(&buf[..len]),
                        );
                        // The kernel often holds a burst behind the datagram
                        // that woke us. Eating it here lets one `drain` — one
                        // timer, one transmit sweep, one event pass — serve the
                        // whole burst instead of one per datagram.
                        for _ in 0..RECV_BURST {
                            match socket.try_recv_from(&mut buf) {
                                Ok((len, remote)) => feed_datagram(
                                    &mut driver,
                                    &channels,
                                    &live,
                                    remote,
                                    BytesMut::from(&buf[..len]),
                                ),
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                                Err(error) => {
                                    tracing::debug!(%error, "the socket could not receive");
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the socket could not receive");
                    }
                }
            }
            () = notify.notified() => {
                // A stream wrote into the engine: `poll_transmit` below has
                // something to send.
            }
            () = &mut sleep => {
                // A timer fired: `poll_transmit` below observes it.
            }
        }

        if !drain(
            &send,
            &mut driver,
            &channels,
            &mut waiting,
            &mut waiters,
            &live,
            &connected,
        ) {
            break;
        }
    }
}

/// Move everything the driver wants to send, then report its events.
///
/// Returns `false` when the socket is unusable, which ends the task.
fn drain(
    send: &mpsc::Sender<Outgoing>,
    driver: &mut Driver,
    channels: &Channels,
    waiting: &mut HashMap<ConnectionHandle, Waiting>,
    waiters: &mut HashMap<ConnectionHandle, Vec<oneshot::Sender<()>>>,
    live: &Arc<Mutex<HashSet<ConnectionHandle>>>,
    connected: &Arc<Mutex<HashSet<ConnectionHandle>>>,
) -> bool {
    let now = Instant::now();
    driver.handle_timeout(now);
    while let Some(outgoing) = driver.poll_transmit(now) {
        if send.try_send(outgoing).is_err() {
            // The sender task is behind. The engine has marked this transmit
            // as sent, so dropping it is a loss its recovery re-emits; waiting
            // for the queue here would block receive, which is the spiral this
            // split exists to prevent.
            tracing::debug!("the send queue is full; dropped a transmit for loss recovery");
            break;
        }
    }
    driver.pump_endpoint_events();
    for handle in driver.handles() {
        while let Some(event) = driver.poll_event(handle) {
            match &event {
                Event::Stream(StreamEvent::Opened { dir: Dir::Bi }) => {
                    // The engine reports "one or more"; hand each new stream
                    // to the carrier, which decides what to do with it.
                    if let Some(shared) = driver.shared(handle) {
                        while let Some(stream) = conn::accept(&shared) {
                            if channels.streams.send(stream).is_err() {
                                return false;
                            }
                        }
                    }
                }
                // A stream handle parked one waker; this is what releases it.
                Event::Stream(StreamEvent::Readable { id } | StreamEvent::Finished { id }) => {
                    if let Some(shared) = driver.shared(handle) {
                        shared.wake(*id, Wait::Read);
                    }
                }
                Event::Stream(StreamEvent::Writable { id } | StreamEvent::Stopped { id, .. }) => {
                    if let Some(shared) = driver.shared(handle) {
                        shared.wake(*id, Wait::Write);
                        // A stopped stream releases both ends: the reader must
                        // see the error rather than park forever.
                        shared.wake(*id, Wait::Read);
                    }
                }
                Event::HandshakeDataReady => {
                    // The record layer derives from the handshake hash, and
                    // this is the event that says it exists.
                    if let Some(shared) = driver.shared(handle) {
                        let hash = shared
                            .connection
                            .lock()
                            .crypto_session()
                            .handshake_data()
                            .and_then(|data| data.downcast::<HandshakeData>().ok())
                            .map(|data| data.hash.clone());
                        if let Some(hash) = hash {
                            shared.set_hash(hash);
                        }
                    }
                }
                Event::Connected => {
                    connected.lock().insert(handle);
                    if let Some(waiters) = waiters.remove(&handle) {
                        for waiter in waiters {
                            let _ = waiter.send(());
                        }
                    }
                    // A caller asking for a stream waits for exactly this.
                    if let Some(waiting) = waiting.remove(&handle) {
                        let outcome = match driver.shared(handle) {
                            Some(shared) => conn::open(&shared, waiting.encrypt),
                            None => Err(std::io::Error::new(
                                std::io::ErrorKind::NotConnected,
                                "the connection is gone",
                            )),
                        };
                        let _ = waiting.reply.send(outcome);
                    }
                }
                Event::ConnectionLost { reason } => {
                    // The reason is the only place a dead carrier says why;
                    // the pool's repair treats every reason the same, but a
                    // silent close leaves "the lane stalls" undiagnosable.
                    tracing::debug!(?reason, "the UDP carrier connection was lost");
                    live.lock().remove(&handle);
                    connected.lock().remove(&handle);
                    waiters.remove(&handle);
                    if let Some(shared) = driver.shared(handle) {
                        shared.close();
                        let _ = channels.lost.send(shared.key);
                    }
                    if let Some(waiting) = waiting.remove(&handle) {
                        let _ = waiting.reply.send(Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "the connection was lost before it carried a stream",
                        )));
                    }
                }
                _ => {}
            }
            if channels.events.send((handle, event)).is_err() {
                return false;
            }
        }
    }
    true
}

/// Send one engine transmission.
///
/// A transmission that carries several datagrams goes out as one
/// `UDP_SEGMENT` message on Linux — the kernel builds one skb and segments it
/// — and as one `send_to` per datagram everywhere else, or after the kernel
/// has refused GSO once. The wire is identical either way; only the syscall
/// count differs.
async fn send_outgoing(
    socket: &UdpSocket,
    outgoing: &crate::transport::quic::driver::Outgoing,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(segment) = outgoing.segment_size
        && !GSO_DISABLED.load(std::sync::atomic::Ordering::Relaxed)
    {
        match socket.try_io(tokio::io::Interest::WRITABLE, || {
            gso_sendmsg(socket, outgoing, segment)
        }) {
            Ok(()) => return Ok(()),
            Err(error) if gso_refused(&error) => {
                GSO_DISABLED.store(true, std::sync::atomic::Ordering::Relaxed);
                tracing::debug!(%error, "UDP GSO refused; sending datagrams one at a time");
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error),
        }
    }
    for datagram in outgoing.datagrams() {
        socket.send_to(datagram, outgoing.destination).await?;
    }
    Ok(())
}

/// One `UDP_SEGMENT` message for a multi-datagram transmission.
#[cfg(target_os = "linux")]
fn gso_sendmsg(
    socket: &UdpSocket,
    outgoing: &crate::transport::quic::driver::Outgoing,
    segment: usize,
) -> std::io::Result<()> {
    use std::io::IoSlice;
    use std::os::fd::AsRawFd;

    use nix::sys::socket::{ControlMessage, MsgFlags, SockaddrStorage, sendmsg};

    let segment = u16::try_from(segment).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a datagram too large for UDP GSO",
        )
    })?;
    let iov = [IoSlice::new(&outgoing.contents)];
    let control = [ControlMessage::UdpGsoSegments(&segment)];
    let address = SockaddrStorage::from(outgoing.destination);
    sendmsg(
        socket.as_raw_fd(),
        &iov,
        &control,
        MsgFlags::MSG_DONTWAIT,
        Some(&address),
    )?;
    Ok(())
}

/// Errors that mean "this kernel or path will not take GSO", as opposed to a
/// full send buffer (`WouldBlock`, retried) or a real fault.
#[cfg(target_os = "linux")]
fn gso_refused(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EINVAL | libc::EMSGSIZE | libc::EIO | libc::ENOPROTOOPT | libc::EOPNOTSUPP)
    )
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        reason = "tests expect on values they just constructed"
    )]
    use super::*;
    use crate::config::{MaskedString, NoiseConfig};
    use crate::transport::NoiseKeys;
    use crate::transport::quic::crypto::TokenKeys;
    use crate::transport::quic::session::{NoiseClientConfig, NoiseServerConfig};
    use base64::Engine as _;
    use snow::params::NoiseParams;
    use std::time::Duration;

    fn encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn pair() -> (NoiseConfig, NoiseConfig) {
        let params: NoiseParams = crate::transport::noise::NOISE_PATTERN
            .parse()
            .expect("the pattern is a constant");
        let client = snow::Builder::new(params.clone())
            .generate_keypair()
            .expect("a key pair");
        let server = snow::Builder::new(params)
            .generate_keypair()
            .expect("a key pair");
        (
            NoiseConfig {
                public_key: encode(&server.public),
                private_key: MaskedString::from(encode(&client.private).as_str()),
            },
            NoiseConfig {
                public_key: encode(&client.public),
                private_key: MaskedString::from(encode(&server.private).as_str()),
            },
        )
    }

    #[expect(
        clippy::panic,
        reason = "the deadline arm panics with the test's failure, which is what an assertion is"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "one handshake test covers both protection classes and the event contract"
    )]
    #[tokio::test]
    async fn two_endpoints_complete_the_handshake_over_a_real_socket() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client_cfg, server_cfg) = pair();

        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, mut accepted_rx) = mpsc::unbounded_channel();
        let (server_events_tx, mut server_events_rx) = mpsc::unbounded_channel();
        let (server_streams_tx, mut server_streams_rx) = mpsc::unbounded_channel();
        let server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: server_events_tx,
                streams: server_streams_tx,
            },
        );

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, mut client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let handle = client
            .connect(
                server_addr,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
            )
            .await
            .expect("the client connects");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut client_connected = false;
        let mut server_connected = false;
        while !(client_connected && server_connected) {
            tokio::select! {
                accepted = accepted_rx.recv() => {
                    assert!(accepted.is_some(), "the server task stopped");
                }
                event = client_events_rx.recv() => {
                    let (who, event) = event.expect("the client task stopped");
                    assert_eq!(who, handle, "an event for another connection");
                    if matches!(event, Event::Connected) {
                        client_connected = true;
                    }
                }
                event = server_events_rx.recv() => {
                    let (_who, event) = event.expect("the server task stopped");
                    if matches!(event, Event::Connected) {
                        server_connected = true;
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    panic!("the handshake did not complete");
                }
            }
        }

        // Both protection classes, one connection: the selector byte is what
        // tells the accepting side which one a stream carries.
        for (encrypt, payload) in [(false, &b"plain bytes"[..]), (true, &b"sealed bytes"[..])] {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let mut client_stream = client
                .open_stream(handle, encrypt)
                .await
                .expect("a client stream");
            client_stream
                .write_all(payload)
                .await
                .expect("the client writes");
            client_stream.flush().await.expect("the client flushes");

            let mut server_stream = tokio::time::timeout_at(deadline, server_streams_rx.recv())
                .await
                .expect("the stream arrives")
                .expect("the server task stopped");

            let mut seen = [0u8; 32];
            let read = tokio::time::timeout_at(deadline, server_stream.read(&mut seen))
                .await
                .expect("the server reads")
                .expect("the read succeeds");
            assert_eq!(&seen[..read], payload, "encrypt={encrypt}");

            server_stream
                .write_all(b"pong")
                .await
                .expect("the server answers");
            server_stream.flush().await.expect("the server flushes");

            let mut answer = [0u8; 4];
            let read = tokio::time::timeout_at(deadline, client_stream.read(&mut answer))
                .await
                .expect("the client reads")
                .expect("the read succeeds");
            assert_eq!(&answer[..read], b"pong", "encrypt={encrypt}");
        }
    }
    /// The one-way half of the lane test: does a long encrypted write arrive
    /// intact when nobody writes back?
    #[tokio::test]
    async fn a_long_encrypted_write_arrives_in_order() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const FRAME: usize = 8 * 1024;
        const COUNT: usize = 200;

        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );
        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let mut channel = client
            .connect_stream(
                server_addr,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
                true,
            )
            .await
            .expect("the direct channel comes up");

        let writer = tokio::spawn(async move {
            for i in 0..COUNT {
                let buf = vec![u8::try_from(i).expect("frame indices fit in a byte"); FRAME];
                channel.write_all(&buf).await.expect("the write");
                channel.flush().await.expect("the flush");
            }
            channel.shutdown().await.expect("the shutdown");
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut served = tokio::time::timeout_at(deadline, streams_rx.recv())
            .await
            .expect("the stream arrives")
            .expect("the server task stopped");
        let reader = tokio::spawn(async move {
            let mut seen = vec![0u8; FRAME];
            for i in 0..COUNT {
                served.read_exact(&mut seen).await.expect("the read");
                assert!(
                    seen.iter()
                        .all(|byte| *byte == u8::try_from(i).expect("frame indices fit in a byte")),
                    "frame {i} arrived corrupted at byte {:?}",
                    seen.iter().position(
                        |byte| *byte != u8::try_from(i).expect("frame indices fit in a byte")
                    )
                );
            }
        });
        tokio::time::timeout(Duration::from_secs(20), async {
            writer.await.expect("the writer task");
            reader.await.expect("the reader task");
        })
        .await
        .expect("a long encrypted write must arrive in order");
    }

    /// A lane does not alternate: it reads and writes on the same stream at
    /// the same time, and it keeps writing past the stream's first flow-control
    /// window. That is the shape that parked a writer behind a reader's waker
    /// and the shape a short write leaves pending bytes in, so the test sends
    /// more than one window each way and requires every byte back.
    #[expect(
        clippy::too_many_lines,
        reason = "one test is one scenario: connect, then push more than a window and check every frame"
    )]
    #[tokio::test]
    async fn a_stream_carries_while_it_is_also_writing() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const FRAME: usize = 8 * 1024;
        const COUNT: usize = 200; // 1.6 MiB, beyond one stream window

        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let channel = client
            .connect_stream(
                server_addr,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
                true,
            )
            .await
            .expect("the direct channel comes up");

        let (mut client_read, mut client_write) = tokio::io::split(channel);
        // Opening a QUIC stream is lazy: the peer learns of it from the first
        // STREAM frame, so the writer starts before the server awaits the
        // accept. A lane does the same — its hello is what opens it.
        let writer = tokio::spawn(async move {
            for i in 0..COUNT {
                let buf = vec![u8::try_from(i).expect("frame indices fit in a byte"); FRAME];
                client_write.write_all(&buf).await.expect("the write");
                client_write.flush().await.expect("the flush");
            }
            client_write.shutdown().await.expect("the shutdown");
        });

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let served = tokio::time::timeout_at(deadline, streams_rx.recv())
            .await
            .expect("the stream arrives")
            .expect("the server task stopped");
        let (mut server_read, mut server_write) = tokio::io::split(served);
        let reader = tokio::spawn(async move {
            let mut seen = vec![0u8; FRAME];
            for i in 0..COUNT {
                server_read.read_exact(&mut seen).await.expect("the read");
                assert!(
                    seen.iter()
                        .all(|byte| *byte == u8::try_from(i).expect("frame indices fit in a byte")),
                    "frame {i} arrived corrupted"
                );
                server_write.write_all(&seen).await.expect("the echo");
                server_write.flush().await.expect("the echo flush");
            }
            server_write.shutdown().await.expect("the echo shutdown");
        });
        let answers = tokio::spawn(async move {
            let mut answer = vec![0u8; FRAME];
            for i in 0..COUNT {
                client_read
                    .read_exact(&mut answer)
                    .await
                    .expect("the answer");
                assert!(
                    answer
                        .iter()
                        .all(|byte| *byte == u8::try_from(i).expect("frame indices fit in a byte")),
                    "answer {i} arrived corrupted"
                );
            }
        });

        tokio::time::timeout(Duration::from_secs(20), async {
            writer.await.expect("the writer task");
            reader.await.expect("the echo task");
            answers.await.expect("the answer task");
        })
        .await
        .expect("a stream must carry in both directions at once");
    }

    /// A pool tunnel is one connection carrying many streams: opening three
    /// must produce three independent streams on the peer, not three
    /// connections.
    #[tokio::test]
    async fn one_connection_carries_several_streams() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );
        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let handle = client
            .connect(
                server_addr,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
            )
            .await
            .expect("the client connects");
        client
            .await_connected(handle)
            .await
            .expect("the handshake completes");

        for i in 0..3u8 {
            let mut stream = client.open_stream(handle, false).await.expect("a stream");
            stream.write_all(&[i; 64]).await.expect("the write");
            stream.flush().await.expect("the flush");
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for expected in 0..3u8 {
            let mut stream = tokio::time::timeout_at(deadline, streams_rx.recv())
                .await
                .expect("the stream arrives")
                .expect("the server task stopped");
            let mut seen = [0u8; 64];
            tokio::time::timeout_at(deadline, stream.read_exact(&mut seen))
                .await
                .expect("the read completes")
                .expect("the read succeeds");
            assert_eq!(seen, [expected; 64]);
        }
    }

    #[tokio::test]
    async fn a_direct_channel_is_one_stream_over_udp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );

        // One call: connect, complete the handshake, open one stream.
        let mut channel = client
            .connect_stream(
                server_addr,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
                true,
            )
            .await
            .expect("the direct channel comes up");

        channel
            .write_all(b"the visitor's bytes")
            .await
            .expect("the client writes");
        channel.flush().await.expect("the client flushes");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut served = tokio::time::timeout_at(deadline, streams_rx.recv())
            .await
            .expect("the stream arrives")
            .expect("the server task stopped");
        let mut seen = [0u8; 32];
        let read = tokio::time::timeout_at(deadline, served.read(&mut seen))
            .await
            .expect("the server reads")
            .expect("the read succeeds");
        assert_eq!(&seen[..read], b"the visitor's bytes");

        served.write_all(b"pong").await.expect("the server answers");
        served.flush().await.expect("the server flushes");

        let mut answer = [0u8; 4];
        let read = tokio::time::timeout_at(deadline, channel.read(&mut answer))
            .await
            .expect("the client reads")
            .expect("the read succeeds");
        assert_eq!(&answer[..read], b"pong");
    }

    /// One direction's loss pattern: one skb in `one_in` is dropped, and a
    /// dropped skb takes `burst` datagrams with it.
    ///
    /// netem drops a *segment batch* as one skb, so a carrier that fills the
    /// path with eight-datagram batches pays eight consecutive losses for one
    /// netem drop — the difference between a 1 % path and an 8 % one, in the
    /// worst shape for a packet-threshold loss detector.
    struct Loss {
        rng: u64,
        one_in: u64,
        burst: usize,
        skip: usize,
        drops: u64,
    }

    impl Loss {
        fn new(one_in: u64, burst: usize) -> Self {
            Self {
                rng: 0x9E37_79B9_7F4A_7C15,
                one_in,
                burst: burst.max(1),
                skip: 0,
                drops: 0,
            }
        }

        /// Whether this datagram survives.
        fn keep(&mut self) -> bool {
            if self.skip > 0 {
                self.skip -= 1;
                self.drops += 1;
                return false;
            }
            self.rng = self
                .rng
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            if (self.rng >> 33) % self.one_in == 0 {
                self.skip = self.burst - 1;
                self.drops += 1;
                return false;
            }
            true
        }
    }

    /// A link that delays every datagram and drops one in a hundred.
    ///
    /// The bench imposes the same shape with netem and two namespaces, which
    /// takes minutes and moves the loss into the kernel. This one is a fifth of
    /// a millisecond away from a panic message: two engines, real sockets, and
    /// the path in the middle, so a transport stall is reproducible without the
    /// harness.
    async fn lossy_relay(
        front: UdpSocket,
        back: UdpSocket,
        server: SocketAddr,
        delay: Duration,
        drop_one_in: u64,
        burst: usize,
        dropped: Arc<std::sync::atomic::AtomicU64>,
    ) {
        use std::collections::VecDeque;
        use std::sync::atomic::Ordering;

        // Two buffers: `select!` holds both arms' borrows at once.
        let mut from_client = vec![0u8; 64 * 1024];
        let mut from_server = vec![0u8; 64 * 1024];
        let mut client: Option<SocketAddr> = None;
        let mut to_server: VecDeque<(std::time::Instant, Vec<u8>)> = VecDeque::new();
        let mut to_client: VecDeque<(std::time::Instant, Vec<u8>)> = VecDeque::new();
        // The batch a bulk sender fills the path with is on the way to the
        // server; the acknowledgements come back one small datagram at a time.
        let mut towards_server = Loss::new(drop_one_in, burst);
        let mut towards_client = Loss::new(drop_one_in, 1);
        loop {
            let now = std::time::Instant::now();
            loop {
                let Some((due, _)) = to_server.front() else {
                    break;
                };
                if *due > now {
                    break;
                }
                let Some((_, bytes)) = to_server.pop_front() else {
                    break;
                };
                let _ = back.send_to(&bytes, server).await;
            }
            loop {
                let Some((due, _)) = to_client.front() else {
                    break;
                };
                if *due > now {
                    break;
                }
                let Some((_, bytes)) = to_client.pop_front() else {
                    break;
                };
                if let Some(client) = client {
                    let _ = front.send_to(&bytes, client).await;
                }
            }
            let next = to_server
                .front()
                .map(|(due, _)| *due)
                .into_iter()
                .chain(to_client.front().map(|(due, _)| *due))
                .min()
                .unwrap_or_else(|| now + Duration::from_millis(1));
            let sleep = tokio::time::sleep_until(tokio::time::Instant::from_std(next));
            tokio::pin!(sleep);
            tokio::select! {
                received = front.recv_from(&mut from_client) => {
                    if let Ok((len, from)) = received {
                        client = Some(from);
                        if towards_server.keep() {
                            to_server.push_back((
                                std::time::Instant::now() + delay,
                                from_client[..len].to_vec(),
                            ));
                        }
                    }
                }
                received = back.recv_from(&mut from_server) => {
                    if let Ok((len, _)) = received {
                        if towards_client.keep() {
                            to_client.push_back((
                                std::time::Instant::now() + delay,
                                from_server[..len].to_vec(),
                            ));
                        }
                    }
                }
                () = &mut sleep => {}
            }
            dropped.store(
                towards_server.drops + towards_client.drops,
                Ordering::Relaxed,
            );
        }
    }

    /// The loss1 cell in one process: one bulk stream, two engines, and a
    /// 10 ms path that loses one datagram in a hundred.
    ///
    /// Ignored by default because it *measures*: the floor below is a budget
    /// for a carrier that has to replace KCP, and a machine under load says
    /// nothing about the code. Run it with `--ignored --nocapture`:
    /// `cargo test --features quic --lib
    /// transport::quic::socket::tests::a_stream_carries_over_a_lossy_link`
    #[expect(
        clippy::too_many_lines,
        reason = "one measurement: the relay, the transfer and the report belong together"
    )]
    #[ignore = "measures throughput over a lossy path; run with --ignored --nocapture"]
    #[tokio::test]
    async fn a_stream_carries_over_a_lossy_link() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // What the stream must carry, one way.
        const TOTAL: usize = 8 * 1024 * 1024;
        // The write size an IP frame's worth of bytes would be.
        const CHUNK: usize = 8 * 1024;
        // A floor no carrier that replaces KCP may fall under on this path.
        const FLOOR_MBIT: u128 = 50;

        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let mut server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        // The same transport parameters the carrier ships with: a test that
        // measured the crate's defaults would measure the wrong controller.
        server_config.transport_config(crate::transport::quic::params::transport_config());
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );

        // The path: the client dials front, whose datagrams leave from back,
        // so the server's peer is the relay and the relay knows where to send
        // the answers.
        let front = UdpSocket::bind("127.0.0.1:0").await.expect("the front");
        let relay_addr = front.local_addr().expect("an address");
        let back = UdpSocket::bind("127.0.0.1:0").await.expect("the back");
        // `burst` is the number of datagrams one netem drop costs. On the
        // bench that is the sender's segment batch, which is what a 1 % skb
        // loss becomes on the wire; here it is a knob so the two shapes can be
        // compared without re-running the harness.
        let burst: usize = std::env::var("MOLEHILL_TEST_LOSS_BURST")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let dropped = Arc::new(AtomicU64::new(0));
        tokio::spawn(lossy_relay(
            front,
            back,
            server_addr,
            Duration::from_millis(10),
            100,
            burst,
            Arc::clone(&dropped),
        ));

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let mut config = ClientConfig::new(Arc::new(NoiseClientConfig::new(
            NoiseKeys::from_config(&client_cfg).expect("client keys"),
        )));
        config.transport_config(crate::transport::quic::params::transport_config());
        let mut channel = client
            .connect_stream(relay_addr, config, true)
            .await
            .expect("the direct channel comes up over the lossy path");

        let started = std::time::Instant::now();
        let shared = channel.shared_weak();
        let reporter = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            loop {
                ticker.tick().await;
                let Some(shared) = shared.upgrade() else {
                    break;
                };
                let (rtt, cwnd, lost, sent, streams) = shared.engine_stats();
                println!(
                    "  stats rtt={}ms cwnd={} lost={} sent={} streams={streams}",
                    rtt.as_millis(),
                    cwnd,
                    lost,
                    sent
                );
            }
        });
        let writer = tokio::spawn(async move {
            let buf = vec![0x5Au8; CHUNK];
            for _ in 0..TOTAL / CHUNK {
                channel.write_all(&buf).await.expect("the write");
            }
            channel.shutdown().await.expect("the shutdown");
        });

        let mut served = tokio::time::timeout(Duration::from_secs(5), streams_rx.recv())
            .await
            .expect("the stream arrives")
            .expect("the server task stopped");
        let got = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&got);
        let read = tokio::time::timeout(Duration::from_secs(20), async move {
            let mut seen = vec![0u8; 64 * 1024];
            while counter.load(Ordering::Relaxed) < TOTAL {
                let read = served.read(&mut seen).await.expect("the read");
                if read == 0 {
                    break;
                }
                counter.fetch_add(read, Ordering::Relaxed);
            }
        })
        .await;
        let elapsed = started.elapsed();
        let received = got.load(Ordering::Relaxed);
        // Mbit/s without a float: bits over microseconds is already the unit.
        let mbit = u128::try_from(received).expect("a byte count") * 8 / elapsed.as_micros().max(1);
        println!(
            "lossy carrier (burst={burst}): {received} of {TOTAL} bytes in {elapsed:?} = {mbit} Mbit/s, {} datagrams dropped, read finished={}",
            dropped.load(Ordering::Relaxed),
            read.is_ok()
        );
        writer.abort();
        reporter.abort();
        assert!(
            mbit >= FLOOR_MBIT,
            "the carrier carried {mbit} Mbit/s over a 1 % loss path, below the {FLOOR_MBIT} Mbit/s a KCP replacement has to keep"
        );
    }

    /// A frame the engine accepts only in part must still reach the peer
    /// without a second write.
    ///
    /// `poll_write` hands the engine one framed chunk, and the engine takes
    /// only what its send window allows; the rest waits in the handle. If the
    /// call reports the caller's bytes as written at that point, the tail is
    /// stranded: `write_all` returns, no further write comes, and the peer
    /// waits for the rest of a frame that never leaves. A lane does exactly
    /// this — write a batch, then wait for the answer — so the failure shows up
    /// as a carrier that carries nothing while both engines are idle.
    #[tokio::test]
    async fn a_partially_accepted_frame_still_reaches_the_peer() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const TOTAL: usize = 10 * 1024;

        // 8 KiB of send credit against a 10 KiB write: the engine has to take
        // the frame in two bites.
        let mut transport = quinn_proto::TransportConfig::default();
        transport
            .send_window(8 * 1024)
            .stream_receive_window(quinn_proto::VarInt::from_u32(64 * 1024))
            .receive_window(quinn_proto::VarInt::from_u32(128 * 1024));
        let transport = Arc::new(transport);

        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let mut server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        server_config.transport_config(Arc::clone(&transport));
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let mut config = ClientConfig::new(Arc::new(NoiseClientConfig::new(
            NoiseKeys::from_config(&client_cfg).expect("client keys"),
        )));
        config.transport_config(transport);
        let mut channel = client
            .connect_stream(server_addr, config, false)
            .await
            .expect("the direct channel comes up");

        channel
            .write_all(&vec![0xA5u8; TOTAL])
            .await
            .expect("the write");
        // Deliberately no flush: `write_all` has reported every byte as
        // written, so the caller has nothing left to ask for.

        let mut served = tokio::time::timeout(Duration::from_secs(5), streams_rx.recv())
            .await
            .expect("the stream arrives")
            .expect("the server task stopped");
        let mut seen = vec![0u8; TOTAL];
        tokio::time::timeout(Duration::from_secs(5), served.read_exact(&mut seen))
            .await
            .expect("the peer never saw the tail of a frame the engine took in part")
            .expect("the read succeeds");
        assert!(seen.iter().all(|byte| *byte == 0xA5));
    }

    /// The carrier's own ceiling: one stream, one bulk writer, loopback, no
    /// relay and no loss.
    ///
    /// The bench's numbers are per-byte *path* numbers, hub and TUN included;
    /// this is the carrier alone, so a per-byte cost that shows up here is the
    /// carrier's and one that does not is the L3 path's.
    #[expect(
        clippy::too_many_lines,
        reason = "one measurement: harness, transfer and report belong together"
    )]
    #[ignore = "measures the carrier's own throughput; run with --ignored --nocapture"]
    #[tokio::test]
    async fn a_stream_carries_at_full_speed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        const TOTAL: usize = 256 * 1024 * 1024;
        const CHUNK: usize = 64 * 1024;

        let (client_cfg, server_cfg) = pair();
        let server_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted_rx) = mpsc::unbounded_channel();
        let (events_tx, _events_rx) = mpsc::unbounded_channel();
        let (streams_tx, mut streams_rx) = mpsc::unbounded_channel();
        let mut server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        server_config.transport_config(crate::transport::quic::params::transport_config());
        let (_server, _server_task) = spawn(
            server_socket,
            Some(Arc::new(server_config)),
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: accepted_tx,
                events: events_tx,
                streams: streams_tx,
            },
        );

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams_rx) = mpsc::unbounded_channel();
        let (client_events_tx, _client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                lost: {
                    let (lost, _) = mpsc::unbounded_channel();
                    lost
                },
                accepted: unused_tx,
                events: client_events_tx,
                streams: unused_streams_tx,
            },
        );
        let mut config = ClientConfig::new(Arc::new(NoiseClientConfig::new(
            NoiseKeys::from_config(&client_cfg).expect("client keys"),
        )));
        config.transport_config(crate::transport::quic::params::transport_config());
        let mut channel = client
            .connect_stream(server_addr, config, false)
            .await
            .expect("the direct channel comes up");

        let started = std::time::Instant::now();
        let writer = tokio::spawn(async move {
            let buf = vec![0x5Au8; CHUNK];
            for _ in 0..TOTAL / CHUNK {
                channel.write_all(&buf).await.expect("the write");
            }
            channel.shutdown().await.expect("the shutdown");
        });

        let mut served = tokio::time::timeout(Duration::from_secs(10), streams_rx.recv())
            .await
            .expect("the stream arrives")
            .expect("the server task stopped");
        let got = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&got);
        let read = tokio::time::timeout(Duration::from_secs(30), async move {
            let mut seen = vec![0u8; CHUNK];
            while counter.load(Ordering::Relaxed) < TOTAL {
                let read = served.read(&mut seen).await.expect("the read");
                if read == 0 {
                    break;
                }
                counter.fetch_add(read, Ordering::Relaxed);
            }
        })
        .await;
        let elapsed = started.elapsed();
        let received = got.load(Ordering::Relaxed);
        let mbit = u128::try_from(received).expect("a byte count") * 8 / elapsed.as_micros().max(1);
        println!(
            "carrier ceiling: {received} of {TOTAL} bytes in {elapsed:?} = {mbit} Mbit/s, read finished={}",
            read.is_ok()
        );
        writer.abort();
        assert!(read.is_ok(), "the transfer did not finish");
    }
}
