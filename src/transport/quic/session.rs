//! The engine's handshake seam: Noise KK, and nothing else.
//!
//! `quinn-proto` asks its `Session` for three things a TLS stack normally
//! provides: the packets' keys, the CRYPTO bytes of a handshake, and the
//! peer's transport parameters. This module answers all three from the
//! product's own handshake — the same KK pattern, the same key material, the
//! same builder the TCP carrier uses (`NoiseKeys`) — and hands back
//! pass-through packet keys, because `data_enable` (not the transport) is
//! what decides whether a stream's bytes are encrypted.
//!
//! The transport parameters ride as the Noise handshake's *payload*: one copy
//! each way, inside the handshake's own framing, which is what a TLS
//! extension costs as well. `TransportParameters::write`/`read` are public,
//! so no encoder lives here.

use std::any::Any;
use std::sync::Arc;

use parking_lot::Mutex;
use quinn_proto::crypto::{ClientConfig, ExportKeyingMaterialError, Keys, ServerConfig, Session};
use quinn_proto::transport_parameters::TransportParameters;
use quinn_proto::{ConnectError, ConnectionId, Side, TransportError};

use super::crypto::plain_keys;
use crate::transport::NoiseKeys;

/// The alert code reported when the handshake itself fails: RFC 9001 §4.8
/// maps `0x100 + alert` onto `CRYPTO_ERROR`, and `0x50` is `internal_error`.
const ALERT_INTERNAL_ERROR: u8 = 0x50;

/// Flight buffer: KK's messages are a few hundred bytes; the transport
/// parameters are the only payload either side adds.
const MAX_HANDSHAKE: usize = 4096;

/// The peer's payload buffer on read.
const MAX_PAYLOAD: usize = 1024;

/// The one byte each side sends in the Handshake packet space once Noise is
/// done.
///
/// `quinn-proto` promotes a connection to "established" only while handling a
/// *Handshake* packet (it mirrors TLS, where both peers exchange Finished).
/// Noise KK is two messages, so after them neither peer would ever send a
/// Handshake packet again and both would wait forever. This marker is that
/// missing flight: one byte, sent after the keys are installed, which the peer
/// consumes without touching the Noise state.
const HANDSHAKE_DONE: u8 = 0xa5;

/// What a completed handshake publishes to the connection.
///
/// The handshake hash is the seed the per-stream record layer derives from
/// (see `stream.rs`); nothing else about the session leaves this module.
#[derive(Debug)]
pub(crate) struct HandshakeData {
    pub(crate) hash: Vec<u8>,
}

struct Inner {
    state: snow::HandshakeState,
    /// Handshake bytes produced by the last read, waiting to be flushed.
    pending: Vec<u8>,
    /// The peer's transport parameters, exactly as they arrived.
    theirs: Option<Vec<u8>>,
    complete: bool,
    keys_handed: bool,
    hash: Vec<u8>,
}

/// One QUIC connection's Noise handshake.
pub(crate) struct NoiseSession {
    /// `None` when the handshake state could not be built: the engine then
    /// sees a session that never completes instead of a panic.
    inner: Mutex<Option<Inner>>,
    side: Side,
    /// Our transport parameters, encoded once at construction.
    ours: Vec<u8>,
}

impl NoiseSession {
    fn new(keys: &NoiseKeys, side: Side, params: &TransportParameters) -> Self {
        let mut ours = Vec::new();
        params.write(&mut ours);
        let mut inner = match keys.handshake_state(side == Side::Client) {
            Ok(state) => Some(Inner {
                state,
                pending: Vec::new(),
                theirs: None,
                complete: false,
                keys_handed: false,
                hash: Vec::new(),
            }),
            Err(error) => {
                // The configuration layer validates the key material, so this
                // is a programming error rather than a network condition.
                tracing::debug!(%error, "QUIC handshake state could not be built");
                None
            }
        };
        // The client speaks first, with its parameters as the payload.
        if side == Side::Client {
            let flight = inner
                .as_mut()
                .map(|state| write_flight(&mut state.state, &ours));
            match flight {
                Some(Ok(bytes)) => {
                    if let Some(state) = inner.as_mut() {
                        state.pending = bytes;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "QUIC handshake flight could not be built");
                    inner = None;
                }
                None => {}
            }
        }
        NoiseSession {
            inner: Mutex::new(inner),
            side,
            ours,
        }
    }
}

/// The error both sides report when the handshake itself fails.
///
/// RFC 9001 §4.8 maps a TLS alert `x` onto `CRYPTO_ERROR` (`0x100 + x`), so
/// the peer sees a protocol failure rather than a generic close.
fn handshake_failure() -> TransportError {
    TransportError {
        code: quinn_proto::TransportErrorCode::crypto(ALERT_INTERNAL_ERROR),
        frame: None,
        reason: "the Noise handshake failed".to_string(),
    }
}

/// Run one `write_message` and return the bytes it produced.
fn write_flight(state: &mut snow::HandshakeState, payload: &[u8]) -> Result<Vec<u8>, snow::Error> {
    let mut buf = vec![0u8; MAX_HANDSHAKE];
    let len = state.write_message(payload, &mut buf)?;
    buf.truncate(len);
    Ok(buf)
}

impl Session for NoiseSession {
    fn initial_keys(&self, _dst_cid: &ConnectionId, _side: Side) -> Keys {
        plain_keys()
    }

    fn handshake_data(&self) -> Option<Box<dyn Any>> {
        let inner = self.inner.lock();
        let state = inner.as_ref()?;
        state.complete.then(|| {
            Box::new(HandshakeData {
                hash: state.hash.clone(),
            }) as Box<dyn Any>
        })
    }

    fn peer_identity(&self) -> Option<Box<dyn Any>> {
        None
    }

    /// No 0-RTT: a reconnecting client has nothing cached, and the record
    /// layer's keys come from this handshake alone.
    fn early_crypto(
        &self,
    ) -> Option<(
        Box<dyn quinn_proto::crypto::HeaderKey>,
        Box<dyn quinn_proto::crypto::PacketKey>,
    )> {
        None
    }

    fn early_data_accepted(&self) -> Option<bool> {
        None
    }

    fn is_handshaking(&self) -> bool {
        let inner = self.inner.lock();
        inner.as_ref().is_none_or(|state| !state.complete)
    }

    fn read_handshake(&mut self, buf: &[u8]) -> Result<bool, TransportError> {
        let mut inner = self.inner.lock();
        let Some(state) = inner.as_mut() else {
            return Err(handshake_failure());
        };
        // The peer's completion marker: not Noise bytes, just the packet the
        // engine needs to see in the Handshake space.
        if state.complete && buf == [HANDSHAKE_DONE] {
            return Ok(true);
        }
        let mut payload = vec![0u8; MAX_PAYLOAD];
        let read = state
            .state
            .read_message(buf, &mut payload)
            .map_err(|error| TransportError {
                code: quinn_proto::TransportErrorCode::crypto(ALERT_INTERNAL_ERROR),
                frame: None,
                reason: format!("the Noise handshake failed: {error}"),
            })?;
        payload.truncate(read);
        state.theirs = Some(payload);

        // KK is a two-message pattern once the static keys are pre-shared:
        // `e, es, ss` one way and `e, ee, se` back. Only the side that is
        // still mid-handshake owes a flight, and only the responder ever does
        // (its parameters ride msg2); the client reading msg2 is finished.
        if !state.state.is_handshake_finished() && self.side == Side::Server {
            state.pending =
                write_flight(&mut state.state, &self.ours).map_err(|_| handshake_failure())?;
        }
        if state.state.is_handshake_finished() {
            state.complete = true;
            state.hash = state.state.get_handshake_hash().to_vec();
        }
        Ok(state.complete)
    }

    fn transport_parameters(&self) -> Result<Option<TransportParameters>, TransportError> {
        let inner = self.inner.lock();
        let Some(state) = inner.as_ref() else {
            return Ok(None);
        };
        let Some(theirs) = state.theirs.as_ref() else {
            return Ok(None);
        };
        let mut buf: &[u8] = theirs;
        let params =
            TransportParameters::read(self.side, &mut buf).map_err(|_| handshake_failure())?;
        Ok(Some(params))
    }

    fn write_handshake(&mut self, buf: &mut Vec<u8>) -> Option<Keys> {
        let mut inner = self.inner.lock();
        let state = inner.as_mut()?;
        // Whatever was queued for *this* call goes first: bytes a read
        // produced, or a marker queued when the keys were handed over.
        buf.extend_from_slice(&state.pending);
        state.pending.clear();

        if state.complete && !state.keys_handed {
            state.keys_handed = true;
            // The marker is queued for the *next* call: only after the engine
            // has installed these keys is a packet encoded in the Handshake
            // space, which is where the peer needs to see it.
            state.pending.push(HANDSHAKE_DONE);
            return Some(plain_keys());
        }
        None
    }

    /// No key update: there is no packet key to rotate.
    fn next_1rtt_keys(
        &mut self,
    ) -> Option<quinn_proto::crypto::KeyPair<Box<dyn quinn_proto::crypto::PacketKey>>> {
        None
    }

    /// Retry is never sent, so no tag can ever validate.
    fn is_valid_retry(
        &self,
        _orig_dst_cid: &ConnectionId,
        _header: &[u8],
        _payload: &[u8],
    ) -> bool {
        false
    }

    fn export_keying_material(
        &self,
        output: &mut [u8],
        _label: &[u8],
        _context: &[u8],
    ) -> Result<(), ExportKeyingMaterialError> {
        output.fill(0);
        Err(ExportKeyingMaterialError)
    }
}

/// The client's half of the seam.
pub(crate) struct NoiseClientConfig {
    keys: NoiseKeys,
}

impl NoiseClientConfig {
    pub(crate) fn new(keys: NoiseKeys) -> Self {
        NoiseClientConfig { keys }
    }
}

impl ClientConfig for NoiseClientConfig {
    fn start_session(
        self: Arc<Self>,
        _version: u32,
        _server_name: &str,
        params: &TransportParameters,
    ) -> Result<Box<dyn Session>, ConnectError> {
        Ok(Box::new(NoiseSession::new(
            &self.keys,
            Side::Client,
            params,
        )))
    }
}

/// The server's half of the seam.
pub(crate) struct NoiseServerConfig {
    keys: NoiseKeys,
}

impl NoiseServerConfig {
    pub(crate) fn new(keys: NoiseKeys) -> Self {
        NoiseServerConfig { keys }
    }
}

impl ServerConfig for NoiseServerConfig {
    fn initial_keys(
        &self,
        _version: u32,
        _dst_cid: &ConnectionId,
    ) -> Result<Keys, quinn_proto::crypto::UnsupportedVersion> {
        Ok(plain_keys())
    }

    /// Unused: this carrier never sends a Retry, so an address is never
    /// validated by one.
    fn retry_tag(&self, _version: u32, _orig_dst_cid: &ConnectionId, _packet: &[u8]) -> [u8; 16] {
        [0u8; 16]
    }

    fn start_session(
        self: Arc<Self>,
        _version: u32,
        params: &TransportParameters,
    ) -> Box<dyn Session> {
        Box::new(NoiseSession::new(&self.keys, Side::Server, params))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MaskedString, NoiseConfig};
    use base64::Engine as _;
    use snow::params::NoiseParams;

    fn encode(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// A client/server pair, generated rather than fixed: the handshake only
    /// needs the two sides to agree on the static keys.
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

    /// `initial_max_streams_bidi` (0x08) = 56, hand-encoded: the crate's writer
    /// is public but its constructor is not, so the test enters through
    /// `read` — the same door the session uses for the peer's parameters.
    fn parameters() -> TransportParameters {
        let bytes = [0x08u8, 0x01, 0x38];
        let mut buf: &[u8] = &bytes;
        TransportParameters::read(Side::Client, &mut buf).expect("the parameters are well formed")
    }

    fn encode_parameters(params: &TransportParameters) -> Vec<u8> {
        let mut encoded = Vec::new();
        params.write(&mut encoded);
        encoded
    }

    #[test]
    fn the_handshake_completes_and_exchanges_the_parameters() {
        let (client_cfg, server_cfg) = pair();
        let client_keys = NoiseKeys::from_config(&client_cfg).expect("client keys");
        let server_keys = NoiseKeys::from_config(&server_cfg).expect("server keys");
        let params = parameters();
        let mut client: Box<dyn Session> =
            Box::new(NoiseSession::new(&client_keys, Side::Client, &params));
        let mut server: Box<dyn Session> =
            Box::new(NoiseSession::new(&server_keys, Side::Server, &params));

        let mut wire = Vec::new();
        client.write_handshake(&mut wire);
        assert!(!wire.is_empty(), "the client speaks first");
        server.read_handshake(&wire).expect("the server reads msg1");

        wire.clear();
        server.write_handshake(&mut wire);
        assert!(!wire.is_empty(), "the server answers");
        client.read_handshake(&wire).expect("the client reads msg2");

        assert!(
            !server.is_handshaking(),
            "KK finishes when the responder writes the second message"
        );

        wire.clear();
        assert!(
            client.write_handshake(&mut wire).is_some(),
            "the client's keys are handed over once it has read msg2"
        );
        assert!(
            wire.is_empty(),
            "KK has nothing left for the client to send with its keys"
        );
        assert!(!client.is_handshaking(), "the client is done");

        // The completion marker: one byte, queued for the next call so the
        // engine encodes it in the Handshake space. The engine promotes a
        // connection only on a Handshake packet, and KK has no third message,
        // so both sides send this one.
        wire.clear();
        assert!(client.write_handshake(&mut wire).is_none());
        assert_eq!(wire, vec![HANDSHAKE_DONE], "the client's marker");
        server
            .read_handshake(&wire)
            .expect("the server accepts the marker without touching Noise");

        // The parameter fields are private to the crate, so the value is
        // asserted through the encoding: what we sent must survive the round
        // trip, and it must differ from an all-defaults encoding (where
        // `initial_max_streams_bidi` is zero).
        let defaults = encode_parameters(
            &TransportParameters::read(Side::Client, &mut &[][..])
                .expect("an empty parameter set is valid"),
        );
        for seen in [server.transport_parameters(), client.transport_parameters()] {
            let seen = seen
                .expect("the parameters parse")
                .expect("the parameters arrived");
            assert_ne!(
                encode_parameters(&seen),
                defaults,
                "the peer's parameters arrived intact"
            );
        }

        let a = client
            .handshake_data()
            .expect("the client publishes its data")
            .downcast::<HandshakeData>()
            .expect("the client's data is ours");
        let b = server
            .handshake_data()
            .expect("the server publishes its data")
            .downcast::<HandshakeData>()
            .expect("the server's data is ours");
        assert_eq!(a.hash, b.hash, "both ends derive the same handshake hash");
        assert!(!a.hash.is_empty());
    }

    #[test]
    fn a_wrong_static_key_cannot_complete_the_handshake() {
        let (client_cfg, _) = pair();
        let (_, other_server) = pair();
        let client_keys = NoiseKeys::from_config(&client_cfg).expect("client keys");
        // The server's own pair does not know the client's public key: KK
        // cannot authenticate that direction.
        let stranger_keys = NoiseKeys::from_config(&other_server).expect("server keys");
        let params = parameters();
        let mut client: Box<dyn Session> =
            Box::new(NoiseSession::new(&client_keys, Side::Client, &params));
        let mut stranger: Box<dyn Session> =
            Box::new(NoiseSession::new(&stranger_keys, Side::Server, &params));

        let mut wire = Vec::new();
        client.write_handshake(&mut wire);
        let outcome = stranger.read_handshake(&wire);
        assert!(
            outcome.is_err(),
            "a peer that does not hold the expected key must not finish the handshake"
        );
        assert!(stranger.is_handshaking());
    }
}
