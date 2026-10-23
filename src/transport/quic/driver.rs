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
    AcceptError, ClientConfig, ConnectError, ConnectionHandle, DatagramEvent, Endpoint,
    EndpointConfig, Event, Incoming, ServerConfig,
};

use tokio::sync::Notify;

use super::conn::Shared;

/// How many datagrams one `poll_transmit` may coalesce into a single send.
///
/// The same cap the batching sender uses (`udp_batch`), so a `UDP_SEGMENT`
/// batch is never wider than the syscall's own limit.
pub(crate) const MAX_DATAGRAMS: usize = 8;

/// One datagram the driver wants sent.
pub(crate) struct Outgoing {
    pub(crate) destination: SocketAddr,
    pub(crate) contents: Vec<u8>,
    /// The segment size when `contents` holds more than one datagram that the
    /// engine built for one `poll_transmit`.
    ///
    /// `quinn-proto` batches up to `MAX_DATAGRAMS` datagrams in one buffer and
    /// marks it with this value: the socket must send each segment as its own
    /// datagram (or as one `UDP_SEGMENT` message), never the whole buffer as
    /// one. A concatenation of 1-RTT packets has no framing the peer could
    /// split — a short header extends to the end of the datagram — so sending
    /// it whole is what made a peer answer "invalid frame ID".
    pub(crate) segment_size: Option<usize>,
}

impl Outgoing {
    /// The datagrams this transmission carries, in wire order.
    pub(crate) fn datagrams(&self) -> std::slice::Chunks<'_, u8> {
        // A segment size is only ever present when it is a positive datagram
        // length; treating a malformed zero as "one datagram" keeps `chunks`
        // from panicking on a value the engine should never produce.
        let size = self.segment_size.filter(|size| *size > 0);
        self.contents
            .chunks(size.unwrap_or(self.contents.len().max(1)))
    }
}

pub(crate) struct Driver {
    endpoint: Endpoint,
    connections: BTreeMap<ConnectionHandle, Arc<Shared>>,
    /// The server's handshake configuration, reused for every `accept`.
    server_config: Option<Arc<ServerConfig>>,
    /// A transmit the endpoint produced itself (version negotiation,
    /// stateless reset): it belongs to no connection, so it waits here.
    pending: Vec<Outgoing>,
    /// Told whenever a stream puts bytes into the engine.
    notify: Arc<Notify>,
    /// The next connection key, handed to `Shared` so a listener can group the
    /// streams of one connection without naming an engine type.
    next_key: u64,
    /// The engine's scratch buffer, reused by every call that takes one: it
    /// was allocated per datagram and per transmit, which is the hot path.
    scratch: Vec<u8>,
}

impl Driver {
    pub(crate) fn new(
        server_config: Option<Arc<ServerConfig>>,
        allow_mtud: bool,
        notify: Arc<Notify>,
    ) -> Self {
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
            notify,
            next_key: 1,
            scratch: Vec::new(),
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
        let key = self.next_key;
        self.next_key += 1;
        self.connections.insert(
            handle,
            Shared::new(connection, Arc::clone(&self.notify), key),
        );
        Ok(handle)
    }

    /// Accept the connection an incoming datagram opened.
    pub(crate) fn accept(
        &mut self,
        now: Instant,
        incoming: Incoming,
    ) -> Result<ConnectionHandle, AcceptError> {
        let (handle, connection) =
            self.endpoint
                .accept(incoming, now, &mut self.scratch, self.server_config.clone())?;
        let key = self.next_key;
        self.next_key += 1;
        self.connections.insert(
            handle,
            Shared::new(connection, Arc::clone(&self.notify), key),
        );
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
        let event = self
            .endpoint
            .handle(now, remote, None, None, data, &mut self.scratch);
        match event {
            Some(DatagramEvent::NewConnection(incoming)) => Some(incoming),
            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(shared) = self.connections.get(&handle) {
                    shared.connection.lock().handle_event(event);
                }
                None
            }
            Some(DatagramEvent::Response(transmit)) => {
                self.pending.push(Outgoing {
                    destination: transmit.destination,
                    contents: self.scratch[..transmit.size].to_vec(),
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
        let scratch = &mut self.scratch;
        for shared in self.connections.values() {
            scratch.clear();
            let mut connection = shared.connection.lock();
            if let Some(transmit) = connection.poll_transmit(now, MAX_DATAGRAMS, scratch) {
                return Some(Outgoing {
                    destination: transmit.destination,
                    contents: scratch[..transmit.size].to_vec(),
                    segment_size: transmit.segment_size,
                });
            }
        }
        None
    }

    /// Consume every timer that has come due.
    ///
    /// The engine keeps an expired timer in its set until the driver says so:
    /// without this call `poll_timeout` reports "now" forever and the loop
    /// spins, and a PTO never fires.
    pub(crate) fn handle_timeout(&mut self, now: Instant) {
        for shared in self.connections.values() {
            let mut connection = shared.connection.lock();
            if connection
                .poll_timeout()
                .is_some_and(|deadline| deadline <= now)
            {
                connection.handle_timeout(now);
            }
        }
    }

    /// When the driver next has work to do, whichever connection is earliest.
    pub(crate) fn poll_timeout(&mut self) -> Option<Instant> {
        self.connections
            .values()
            .map(|shared| shared.connection.lock())
            .filter_map(|mut connection| connection.poll_timeout())
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
                let event = self
                    .connections
                    .get(&handle)
                    .and_then(|shared| shared.connection.lock().poll_endpoint_events());
                let Some(event) = event else {
                    break;
                };
                let reply = self.endpoint.handle_event(handle, event);
                if let Some(reply) = reply {
                    if let Some(shared) = self.connections.get(&handle) {
                        shared.connection.lock().handle_event(reply);
                    }
                }
            }
        }
    }

    /// The next event this connection has for its owner.
    pub(crate) fn poll_event(&mut self, handle: ConnectionHandle) -> Option<Event> {
        self.connections.get(&handle)?.connection.lock().poll()
    }

    /// The connection's shared state, for a task that opens or accepts
    /// streams on it.
    pub(crate) fn shared(&self, handle: ConnectionHandle) -> Option<Arc<Shared>> {
        self.connections.get(&handle).cloned()
    }

    /// Every live connection, for a driver that routes events by handle.
    pub(crate) fn handles(&self) -> Vec<ConnectionHandle> {
        self.connections.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap and expect on values they just constructed"
    )]
    use super::*;
    use crate::config::{MaskedString, NoiseConfig};
    use crate::transport::NoiseKeys;
    use crate::transport::quic::crypto::TokenKeys;
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
            for datagram in outgoing.datagrams() {
                if let Some(incoming) = to.on_datagram(now, from_addr, BytesMut::from(datagram)) {
                    accepted = Some(to.accept(now, incoming).expect("the server accepts"));
                }
            }
        }
        from.pump_endpoint_events();
        accepted
    }

    /// A transmission with a segment size carries several datagrams, and each
    /// must leave as its own datagram. Feeding the concatenation to a peer is
    /// what produced "received a frame that was badly formatted".
    #[test]
    fn a_multi_datagram_transmit_is_split_at_its_segment_size() {
        let outgoing = Outgoing {
            destination: "127.0.0.1:4433".parse().expect("an address"),
            contents: (0u8..10).collect(),
            segment_size: Some(4),
        };
        let segments: Vec<&[u8]> = outgoing.datagrams().collect();
        assert_eq!(
            segments,
            [&[0u8, 1, 2, 3][..], &[4, 5, 6, 7][..], &[8, 9][..]]
        );

        let single = Outgoing {
            destination: outgoing.destination,
            contents: (0u8..10).collect(),
            segment_size: None,
        };
        assert_eq!(single.datagrams().count(), 1);
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
        let mut server = Driver::new(
            Some(Arc::new(server_config)),
            false,
            Arc::new(Notify::new()),
        );
        let mut client = Driver::new(None, false, Arc::new(Notify::new()));
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
