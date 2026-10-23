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

use std::sync::Arc;

use bytes::BytesMut;
use quinn_proto::crypto::{
    AeadKey, CryptoError, HandshakeTokenKey, HeaderKey, HmacKey, KeyPair, Keys, PacketKey,
};
use ring::{aead, hmac};

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

/// The endpoint's stateless-reset key.
///
/// `EndpointConfig::new` requires one, and a weak one would be worse than
/// none: every connection advertises a reset token derived from it, so an
/// unkeyed tag would let an off-path attacker tear down connections it has
/// merely observed. This is the only real cryptography the carrier keeps
/// outside Noise, and it protects the endpoint rather than the payload.
pub(crate) struct ResetHmac(hmac::Key);

impl ResetHmac {
    #[expect(
        clippy::expect_used,
        reason = "a stateless-reset key that cannot be keyed is not an endpoint this carrier can \
                  offer; the system random source is the only safe source and its failure is fatal"
    )]
    pub(crate) fn new() -> Arc<Self> {
        // Per process: a reset token only has to be unforgeable for the life
        // of the endpoint that issues it.
        let key = hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
            .expect("the system random source is available");
        Arc::new(ResetHmac(key))
    }
}

impl HmacKey for ResetHmac {
    fn sign(&self, data: &[u8], signature_out: &mut [u8]) {
        let tag = hmac::sign(&self.0, data);
        let len = signature_out.len().min(tag.as_ref().len());
        signature_out[..len].copy_from_slice(&tag.as_ref()[..len]);
    }

    fn signature_len(&self) -> usize {
        hmac::HMAC_SHA256.digest_algorithm().output_len()
    }

    fn verify(&self, data: &[u8], signature: &[u8]) -> Result<(), CryptoError> {
        hmac::verify(&self.0, data, signature).map_err(|_| CryptoError)
    }
}

/// The endpoint's handshake-token keys.
///
/// `ServerConfig::new` requires one. It seals Retry and address-validation
/// tokens, and this carrier never sends a Retry, so in practice it is never
/// handed a token — but a weak one would be a hole the day that changes, and
/// the derivation is four lines. Each key is derived from fresh randomness and
/// used once, which is why the nonce is a constant.
pub(crate) struct TokenKeys {
    master: hmac::Key,
}

impl TokenKeys {
    #[expect(
        clippy::expect_used,
        reason = "the system random source is the only safe source for a token key, and an endpoint \
                  without one cannot validate a Retry it might be asked to issue"
    )]
    pub(crate) fn new() -> Arc<Self> {
        let master = hmac::Key::generate(hmac::HMAC_SHA256, &ring::rand::SystemRandom::new())
            .expect("the system random source is available");
        Arc::new(TokenKeys { master })
    }
}

impl HandshakeTokenKey for TokenKeys {
    #[expect(
        clippy::expect_used,
        reason = "HMAC-SHA256 yields exactly the 32 bytes AES-256 takes, and the key was just \
                  derived from it"
    )]
    fn aead_from_hkdf(&self, random_bytes: &[u8]) -> Box<dyn AeadKey> {
        let derived = hmac::sign(&self.master, random_bytes);
        let key = aead::UnboundKey::new(&aead::AES_256_GCM, derived.as_ref())
            .expect("HMAC-SHA256 yields the 32 bytes AES-256 needs");
        Box::new(TokenAead(aead::LessSafeKey::new(key)))
    }
}

struct TokenAead(aead::LessSafeKey);

impl AeadKey for TokenAead {
    fn seal(&self, data: &mut Vec<u8>, additional_data: &[u8]) -> Result<(), CryptoError> {
        let nonce = aead::Nonce::assume_unique_for_key([0u8; aead::NONCE_LEN]);
        self.0
            .seal_in_place_append_tag(nonce, aead::Aad::from(additional_data), data)
            .map_err(|_| CryptoError)
    }

    fn open<'a>(
        &self,
        data: &'a mut [u8],
        additional_data: &[u8],
    ) -> Result<&'a mut [u8], CryptoError> {
        let nonce = aead::Nonce::assume_unique_for_key([0u8; aead::NONCE_LEN]);
        self.0
            .open_in_place(nonce, aead::Aad::from(additional_data), data)
            .map_err(|_| CryptoError)
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
    #![expect(
        clippy::expect_used,
        reason = "tests expect on values they just constructed"
    )]
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
