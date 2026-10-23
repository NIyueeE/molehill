//! The pass-through half of the engine's crypto seam.
//!
//! RFC 9000 makes packet protection part of the format, and that is what
//! makes QUIC's control frames (ACK, `RESET_STREAM`, `MAX_DATA`, `CONNECTION_CLOSE`)
//! trustworthy. This carrier gives that property up on purpose: the product's
//! single encryption switch is `data_enable`, the payload's confidentiality
//! is the Noise record stream's job, and the engine is here for the
//! recovery, the pacing and the streams.
//!
//! The three pieces below are therefore *empty* rather than absent: a
//! `PacketKey` with `tag_len() == 0` copies nothing, a `HeaderKey` leaves the
//! first byte and the packet number unmasked, and `plain_keys()` hands the
//! engine the same shape it would get from a real cipher. The engine's packet
//! number arithmetic, its sampling budget and its frame layout are untouched,
//! which is exactly why no fork of the crate is needed.

use bytes::BytesMut;
use quinn_proto::crypto::{CryptoError, HeaderKey, KeyPair, Keys, PacketKey};

/// The RFC 9001 §5.4 sample size.
///
/// The transform is a no-op, but the builder pads a packet so the sample
/// exists and the parser reads it at the same offset. Reporting the standard
/// value keeps that arithmetic identical to a real implementation's instead of
/// inventing a second layout.
const SAMPLE_SIZE: usize = 16;

/// A packet key that copies nothing.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PlainPacketKey;

impl PacketKey for PlainPacketKey {
    fn encrypt(&self, _packet: u64, _buf: &mut [u8], _header_len: usize) {}

    fn decrypt(
        &self,
        _packet: u64,
        _header: &[u8],
        _payload: &mut BytesMut,
    ) -> Result<(), CryptoError> {
        Ok(())
    }

    fn tag_len(&self) -> usize {
        0
    }

    /// No key is ever used up: nothing here is a secret, so nothing forces a
    /// key update.
    fn confidentiality_limit(&self) -> u64 {
        u64::MAX
    }

    /// A packet that carries no tag has nothing to fail authentication with,
    /// so no incoming packet can burn this budget.
    fn integrity_limit(&self) -> u64 {
        u64::MAX
    }
}

/// A header key that leaves the first byte and the packet number as they are.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct PlainHeaderKey;

impl HeaderKey for PlainHeaderKey {
    fn decrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}

    fn encrypt(&self, _pn_offset: usize, _packet: &mut [u8]) {}

    fn sample_size(&self) -> usize {
        SAMPLE_SIZE
    }
}

/// The keys for one packet space, pass-through in both directions.
pub(crate) fn plain_keys() -> Keys {
    Keys {
        header: KeyPair {
            local: Box::new(PlainHeaderKey),
            remote: Box::new(PlainHeaderKey),
        },
        packet: KeyPair {
            local: Box::new(PlainPacketKey),
            remote: Box::new(PlainPacketKey),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_packet_key_carries_no_tag_and_rewrites_nothing() {
        let keys = plain_keys();
        assert_eq!(keys.packet.local.tag_len(), 0);
        assert_eq!(keys.packet.local.confidentiality_limit(), u64::MAX);

        let mut payload = BytesMut::from(&b"the visitor's bytes"[..]);
        let before = payload.clone();
        keys.packet.local.encrypt(7, &mut payload, 0);
        assert_eq!(payload, before, "encryption must not touch the payload");
    }

    #[test]
    fn a_plain_packet_key_accepts_anything_it_is_handed() {
        let keys = plain_keys();
        let mut received = BytesMut::from(&b"the visitor's bytes"[..]);
        keys.packet
            .remote
            .decrypt(7, &[], &mut received)
            .expect("a packet with no tag has nothing to fail");
        assert_eq!(&received[..], &b"the visitor's bytes"[..]);
    }

    #[test]
    fn a_plain_header_key_leaves_the_header_alone() {
        let keys = plain_keys();
        assert_eq!(keys.header.local.sample_size(), SAMPLE_SIZE);
        let mut packet = [0x41u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let before = packet;
        keys.header.local.encrypt(1, &mut packet);
        keys.header.remote.decrypt(1, &mut packet);
        assert_eq!(packet, before);
    }
}
