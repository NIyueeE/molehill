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

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, Result, anyhow};
use bytes::BytesMut;
use quinn_proto::{ClientConfig, ConnectionHandle, Event, ServerConfig};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

use super::driver::Driver;

/// A command from the carrier's async surface into the task.
enum Command {
    Connect {
        remote: SocketAddr,
        config: ClientConfig,
        reply: oneshot::Sender<Result<ConnectionHandle, String>>,
    },
}

/// What the task needs to report upward: a connection the peer opened, and
/// every event a connection raised.
pub(crate) struct Channels {
    pub(crate) accepted: mpsc::UnboundedSender<ConnectionHandle>,
    pub(crate) events: mpsc::UnboundedSender<(ConnectionHandle, Event)>,
}

pub(crate) struct EndpointHandle {
    commands: mpsc::Sender<Command>,
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
}

/// Spawn the task and return the handle the carrier talks to.
pub(crate) fn spawn(
    socket: UdpSocket,
    server_config: Option<Arc<ServerConfig>>,
    allow_mtud: bool,
    channels: Channels,
) -> (EndpointHandle, tokio::task::JoinHandle<()>) {
    let (commands, inbox) = mpsc::channel(16);
    let handle = EndpointHandle { commands };
    let task = tokio::spawn(run(socket, server_config, allow_mtud, channels, inbox));
    (handle, task)
}

async fn run(
    socket: UdpSocket,
    server_config: Option<Arc<ServerConfig>>,
    allow_mtud: bool,
    channels: Channels,
    mut commands: mpsc::Receiver<Command>,
) {
    let mut driver = Driver::new(server_config, allow_mtud);
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
            () = &mut sleep => {
                // A timer fired: `poll_transmit` below observes it.
            }
        }

        if !drain(&socket, &mut driver, &channels).await {
            break;
        }
    }
}

/// Move everything the driver wants to send, then report its events.
///
/// Returns `false` when the socket is unusable, which ends the task.
async fn drain(socket: &UdpSocket, driver: &mut Driver, channels: &Channels) -> bool {
    let now = Instant::now();
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
    use crate::transport::quic::{NoiseClientConfig, NoiseServerConfig, TokenKeys};
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
            },
        );

        let client_socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused_rx) = mpsc::unbounded_channel();
        let (client_events_tx, mut client_events_rx) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                accepted: unused_tx,
                events: client_events_tx,
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
    }
}
