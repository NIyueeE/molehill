//! The engine's event loop, without a socket.
//!
//! `quinn-proto` is sans-IO: it never touches a socket, a timer or a thread.
//! This type owns one `Endpoint` and its live `Connection`s, and answers the
//! four questions a driver has to answer — "here is a datagram", "what should
//! I send", "when must I wake next", "what happened on this connection?" — so
//! the tokio task around it and the tests below share one implementation.
//!
//! Nothing here knows about Noise or streams: it moves packets.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use quinn_proto::{
    AcceptError, ClientConfig, ConnectError, Connection, ConnectionHandle, DatagramEvent, Endpoint,
    EndpointConfig, Event, Incoming, ServerConfig,
};

/// How many datagrams one `poll_transmit` may coalesce into a single send.
///
/// The same cap the batching sender uses (`udp_batch`), so a `UDP_SEGMENT`
/// batch is never wider than the syscall's own limit.
pub(crate) const MAX_DATAGRAMS: usize = 8;

/// One datagram the driver wants sent.
pub(crate) struct Outgoing {
    pub(crate) destination: SocketAddr,
    pub(crate) contents: Vec<u8>,
    /// The GSO segment size, when the engine coalesced several datagrams.
    pub(crate) segment_size: Option<usize>,
}

pub(crate) struct Driver {
    endpoint: Endpoint,
    connections: BTreeMap<ConnectionHandle, Connection>,
    /// The server's handshake configuration, reused for every `accept`.
    server_config: Option<Arc<ServerConfig>>,
    /// A transmit the endpoint produced itself (version negotiation,
    /// stateless reset): it belongs to no connection, so it waits here.
    pending: Vec<Outgoing>,
}

impl Driver {
    pub(crate) fn new(server_config: Option<Arc<ServerConfig>>, allow_mtud: bool) -> Self {
        Driver {
            endpoint: Endpoint::new(
                Arc::new(EndpointConfig::new(super::crypto::ResetHmac::new())),
                server_config.clone(),
                allow_mtud,
                None,
            ),
            connections: BTreeMap::new(),
            server_config,
            pending: Vec::new(),
        }
    }

    /// Start an outgoing connection.
    ///
    /// The server name is a constant marker: this handshake is Noise, which
    /// names nothing outside the two static keys.
    pub(crate) fn connect(
        &mut self,
        now: Instant,
        config: ClientConfig,
        remote: SocketAddr,
    ) -> Result<ConnectionHandle, ConnectError> {
        let (handle, connection) = self.endpoint.connect(now, config, remote, "molehill")?;
        self.connections.insert(handle, connection);
        Ok(handle)
    }

    /// Accept the connection an incoming datagram opened.
    pub(crate) fn accept(
        &mut self,
        now: Instant,
        incoming: Incoming,
    ) -> Result<ConnectionHandle, AcceptError> {
        let mut scratch = Vec::new();
        let (handle, connection) =
            self.endpoint
                .accept(incoming, now, &mut scratch, self.server_config.clone())?;
        self.connections.insert(handle, connection);
        Ok(handle)
    }

    /// Feed one received datagram. Returns the incoming connection it opened,
    /// if it opened one: the caller decides whether to accept it.
    pub(crate) fn on_datagram(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        data: BytesMut,
    ) -> Option<Incoming> {
        let mut scratch = Vec::new();
        let event = self
            .endpoint
            .handle(now, remote, None, None, data, &mut scratch);
        match event {
            Some(DatagramEvent::NewConnection(incoming)) => Some(incoming),
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(connection) = self.connections.get_mut(&handle) {
                    connection.handle_event(event);
                }
                None
            }
            Some(DatagramEvent::Response(transmit)) => {
                self.pending.push(Outgoing {
                    destination: transmit.destination,
                    contents: scratch[..transmit.size].to_vec(),
                    segment_size: transmit.segment_size,
                });
                None
            }
            None => None,
        }
    }

    /// The next datagram to send, if any.
    pub(crate) fn poll_transmit(&mut self, now: Instant) -> Option<Outgoing> {
        if !self.pending.is_empty() {
            return Some(self.pending.remove(0));
        }
        let mut scratch = Vec::new();
        for connection in self.connections.values_mut() {
            if let Some(transmit) = connection.poll_transmit(now, MAX_DATAGRAMS, &mut scratch) {
                return Some(Outgoing {
                    destination: transmit.destination,
                    contents: scratch[..transmit.size].to_vec(),
                    segment_size: transmit.segment_size,
                });
            }
        }
        None
    }

    /// When the driver next has work to do, whichever connection is earliest.
    pub(crate) fn poll_timeout(&mut self) -> Option<Instant> {
        self.connections
            .values_mut()
            .filter_map(Connection::poll_timeout)
            .min()
    }

    /// Move endpoint events between the connections and the endpoint.
    ///
    /// A connection raises them (a new connection ID, a reset token), the
    /// endpoint answers, and the answer may go back into the connection. The
    /// order — transmit, endpoint events, then `poll` — is the one the
    /// engine's own documentation prescribes.
    pub(crate) fn pump_endpoint_events(&mut self) {
        let handles: Vec<ConnectionHandle> = self.connections.keys().copied().collect();
        for handle in handles {
            loop {
                let Some(event) = self
                    .connections
                    .get_mut(&handle)
                    .and_then(Connection::poll_endpoint_events)
                else {
                    break;
                };
                let reply = self.endpoint.handle_event(handle, event);
                if let Some(reply) = reply {
                    if let Some(connection) = self.connections.get_mut(&handle) {
                        connection.handle_event(reply);
                    }
                }
            }
        }
    }

    /// The next event this connection has for its owner.
    pub(crate) fn poll_event(&mut self, handle: ConnectionHandle) -> Option<Event> {
        self.connections.get_mut(&handle)?.poll()
    }

    pub(crate) fn connection_mut(&mut self, handle: ConnectionHandle) -> Option<&mut Connection> {
        self.connections.get_mut(&handle)
    }

    /// Every live connection, for a driver that routes events by handle.
    pub(crate) fn handles(&self) -> Vec<ConnectionHandle> {
        self.connections.keys().copied().collect()
    }

    pub(crate) fn remove(&mut self, handle: ConnectionHandle) {
        self.connections.remove(&handle);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MaskedString, NoiseConfig};
    use crate::transport::NoiseKeys;
    use crate::transport::quic::TokenKeys;
    use crate::transport::quic::session::{NoiseClientConfig, NoiseServerConfig};
    use base64::Engine as _;
    use snow::params::NoiseParams;

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

    /// Move every datagram one driver wants to send into the other, accepting
    /// any connection the receiver opens.
    fn pump(
        from: &mut Driver,
        to: &mut Driver,
        from_addr: SocketAddr,
        now: Instant,
    ) -> Option<ConnectionHandle> {
        let mut accepted = None;
        while let Some(outgoing) = from.poll_transmit(now) {
            from.pump_endpoint_events();
            if let Some(incoming) =
                to.on_datagram(now, from_addr, BytesMut::from(&outgoing.contents[..]))
            {
                accepted = Some(to.accept(now, incoming).expect("the server accepts"));
            }
        }
        from.pump_endpoint_events();
        accepted
    }

    #[test]
    fn two_endpoints_complete_the_handshake() {
        let (client_cfg, server_cfg) = pair();
        let server_addr: SocketAddr = "127.0.0.1:4433".parse().expect("an address");
        let client_addr: SocketAddr = "127.0.0.1:5555".parse().expect("an address");

        let server_config = ServerConfig::new(
            Arc::new(NoiseServerConfig::new(
                NoiseKeys::from_config(&server_cfg).expect("server keys"),
            )),
            TokenKeys::new(),
        );
        let mut server = Driver::new(Some(Arc::new(server_config)), false);
        let mut client = Driver::new(None, false);
        let mut clock = Instant::now();
        let handle = client
            .connect(
                clock,
                ClientConfig::new(Arc::new(NoiseClientConfig::new(
                    NoiseKeys::from_config(&client_cfg).expect("client keys"),
                ))),
                server_addr,
            )
            .expect("the client connects");

        let mut client_done = false;
        let mut server_done = false;
        let mut server_handle = None;
        for _ in 0..64 {
            // A virtual clock: pacing and the PTO are timers, and a fixed
            // instant never lets them fire.
            clock += std::time::Duration::from_millis(10);
            if let Some(handle) = pump(&mut client, &mut server, client_addr, clock) {
                server_handle = Some(handle);
            }
            if let Some(handle) = pump(&mut server, &mut client, server_addr, clock) {
                server_handle = Some(handle);
            }
            while let Some(event) = client.poll_event(handle) {
                if matches!(event, Event::Connected) {
                    client_done = true;
                }
            }
            if let Some(handle) = server_handle {
                while let Some(event) = server.poll_event(handle) {
                    if matches!(event, Event::Connected) {
                        server_done = true;
                    }
                }
            }
            if client_done && server_done {
                break;
            }
        }
        assert!(client_done, "the client completes the handshake");
        assert!(server_done, "the server accepts and completes it");
    }
}
