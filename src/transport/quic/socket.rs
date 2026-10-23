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
use super::driver::Driver;
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
        /// data_enable.
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
}

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

/// Spawn the task and return the handle the carrier talks to.
pub(crate) fn spawn(
    socket: UdpSocket,
    server_config: Option<Arc<ServerConfig>>,
    allow_mtud: bool,
    channels: Channels,
) -> (EndpointHandle, tokio::task::JoinHandle<()>) {
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
    let mut driver = Driver::new(server_config, allow_mtud, Arc::clone(&notify));
    let mut waiting: HashMap<ConnectionHandle, Waiting> = HashMap::new();
    let mut waiters: HashMap<ConnectionHandle, Vec<oneshot::Sender<()>>> = HashMap::new();
    let mut buf = vec![0u8; 64 * 1024];
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
                        let data = BytesMut::from(&buf[..len]);
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
            &socket,
            &mut driver,
            &channels,
            &mut waiting,
            &mut waiters,
            &live,
            &connected,
        )
        .await
        {
            break;
        }
    }
}

/// Move everything the driver wants to send, then report its events.
///
/// Returns `false` when the socket is unusable, which ends the task.
async fn drain(
    socket: &UdpSocket,
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
        // Without GSO a coalesced buffer is one datagram, which is exactly
        // what a plain `send_to` does; segmentation only changes how the
        // batch is framed on the wire, not what it contains.
        if let Err(error) = socket
            .send_to(&outgoing.contents, outgoing.destination)
            .await
        {
            tracing::debug!(%error, "the socket could not send");
            return false;
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
                Event::ConnectionLost { .. } => {
                    live.lock().remove(&handle);
                    connected.lock().remove(&handle);
                    waiters.remove(&handle);
                    if let Some(shared) = driver.shared(handle) {
                        shared.close();
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

#[cfg(test)]
mod tests {
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

    #[tokio::test]
    async fn two_endpoints_complete_the_handshake_over_a_real_socket() {
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
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
}
