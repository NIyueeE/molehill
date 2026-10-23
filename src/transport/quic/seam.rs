//! The seam's UDP side.
//!
//! One connection of the endpoint task, seen as a carrier: it hands out QUIC
//! streams, says which carrier it is, and answers "still alive" from the
//! task's own bookkeeping rather than by guessing.
//!
//! Opening a stream before the handshake is done would fail inside the engine,
//! so this adapter waits for the connection to be established first. That wait
//! is the whole reason the endpoint exposes it: a carrier's caller should not
//! have to know that a QUIC stream has a precondition.

use std::sync::Arc;

use quinn_proto::ConnectionHandle;

use super::socket::EndpointHandle;
use crate::transport::carrier::{Carrier, CarrierConn, CarrierStream, StreamError};

pub(crate) struct UdpConn {
    endpoint: Arc<EndpointHandle>,
    handle: ConnectionHandle,
    /// Whether the streams carry the record layer (the service's data_enable).
    encrypt: bool,
}

impl UdpConn {
    pub(crate) fn new(
        endpoint: Arc<EndpointHandle>,
        handle: ConnectionHandle,
        encrypt: bool,
    ) -> Self {
        UdpConn {
            endpoint,
            handle,
            encrypt,
        }
    }
}

#[async_trait::async_trait]
impl CarrierConn for UdpConn {
    async fn open_stream(&self) -> Result<CarrierStream, StreamError> {
        self.endpoint
            .await_connected(self.handle)
            .await
            .map_err(|_| StreamError::Closed)?;
        match self.endpoint.open_stream(self.handle, self.encrypt).await {
            Ok(stream) => Ok(Box::new(stream)),
            Err(error) => Err(match error.downcast_ref::<std::io::Error>() {
                Some(io) if io.kind() == std::io::ErrorKind::WouldBlock => StreamError::Busy,
                _ => StreamError::Closed,
            }),
        }
    }

    fn carrier(&self) -> Carrier {
        // The UDP carrier's slot. It keeps the name of the implementation that
        // fills it today; the rename and the swap land in the same commit.
        Carrier::Kcp
    }

    fn is_alive(&self) -> bool {
        self.endpoint.is_alive(self.handle)
    }

    async fn stats(&self) -> crate::transport::carrier::CarrierStats {
        self.endpoint.stats(self.handle).await.unwrap_or_default()
    }
}

#[cfg(all(test, feature = "multiplex"))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use base64::Engine as _;
    use quinn_proto::{ClientConfig, ServerConfig};
    use snow::params::NoiseParams;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    use super::super::crypto::TokenKeys;
    use super::super::session::{NoiseClientConfig, NoiseServerConfig};
    use super::super::socket::{Channels, spawn};
    use super::UdpConn;
    use crate::config::{MaskedString, NoiseConfig};
    use crate::transport::NoiseKeys;
    use crate::transport::carrier::{CarrierConn, CarrierStream};

    const DEADLINE: Duration = Duration::from_secs(5);

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

    /// The contract every carrier owes the data plane, run as one function so
    /// both carriers are held to the same words.
    async fn a_stream_carries(
        conn: Arc<dyn CarrierConn>,
        mut inbound: mpsc::UnboundedReceiver<CarrierStream>,
    ) -> crate::transport::carrier::CarrierStats {
        let mut out = conn.open_stream().await.expect("a stream");
        out.write_all(b"through the seam").await.expect("the write");
        out.flush().await.expect("the flush");

        let mut served = tokio::time::timeout(DEADLINE, inbound.recv())
            .await
            .expect("the stream arrives")
            .expect("the peer task lives");
        let mut seen = [0u8; 16];
        let read = tokio::time::timeout(DEADLINE, served.read(&mut seen))
            .await
            .expect("the read completes")
            .expect("the read succeeds");
        assert_eq!(&seen[..read], b"through the seam");

        served.write_all(b"back").await.expect("the echo");
        served.flush().await.expect("the flush");

        let mut answer = [0u8; 4];
        let read = tokio::time::timeout(DEADLINE, out.read(&mut answer))
            .await
            .expect("the read completes")
            .expect("the read succeeds");
        assert_eq!(&answer[..read], b"back");
        assert!(conn.is_alive(), "the connection should still be alive");
        conn.stats().await
    }

    #[tokio::test]
    async fn the_tcp_carrier_satisfies_the_seam() {
        let (client_io, server_io) = tokio::io::duplex(64 * 1024);
        let (mux_tx, mut mux_rx) = mpsc::channel(8);
        tokio::spawn(crate::transport::multiplex::run_server_tunnel(
            server_io,
            crate::transport::multiplex::mux_config(),
            mux_tx,
        ));
        let (streams_tx, streams_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(stream) = mux_rx.recv().await {
                if streams_tx.send(Box::new(stream) as CarrierStream).is_err() {
                    break;
                }
            }
        });
        let (_shutdown, shutdown) = tokio::sync::watch::channel(false);
        let conn = crate::transport::tcp::TcpConn::new(client_io, shutdown);
        let stats = a_stream_carries(Arc::new(conn), streams_rx).await;
        assert_eq!(
            stats.lost_packets, 0,
            "the kernel keeps its losses where userspace cannot read them"
        );
    }

    #[tokio::test]
    async fn the_udp_carrier_satisfies_the_seam() {
        let (client_cfg, server_cfg) = pair();

        let server_socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a server socket");
        let server_addr = server_socket.local_addr().expect("an address");
        let (accepted_tx, _accepted) = mpsc::unbounded_channel();
        let (events_tx, _events) = mpsc::unbounded_channel();
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
                events: events_tx,
                streams: server_streams_tx,
            },
        );
        let (streams_tx, streams_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(stream) = server_streams_rx.recv().await {
                if streams_tx.send(Box::new(stream) as CarrierStream).is_err() {
                    break;
                }
            }
        });

        let client_socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("a client socket");
        let (unused_tx, _unused) = mpsc::unbounded_channel();
        let (unused_events_tx, _unused_events) = mpsc::unbounded_channel();
        let (unused_streams_tx, _unused_streams) = mpsc::unbounded_channel();
        let (client, _client_task) = spawn(
            client_socket,
            None,
            false,
            Channels {
                accepted: unused_tx,
                events: unused_events_tx,
                streams: unused_streams_tx,
            },
        );
        let client = Arc::new(client);
        let handle = client
            .connect(
                server_addr,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
            )
            .await
            .expect("the client connects");

        let conn = UdpConn::new(Arc::clone(&client), handle, true);
        let stats = a_stream_carries(Arc::new(conn), streams_rx).await;
        assert!(
            stats.rtt.is_some(),
            "the engine reports the path's round trip"
        );
        assert!(
            stats.streams >= 1,
            "the engine counts the stream that carried the bytes"
        );
    }
}
