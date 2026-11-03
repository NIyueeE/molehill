//! A stream's own key, derived from the lane's handshake instead of a second
//! handshake.
//!
//! A visitor's channel used to pay a full Noise KK before its first byte: two
//! round trips and three Diffie-Hellman operations, measured at **1 771 µs**
//! against an open of 1 779 — the whole of what a channel costs. The lane and
//! the server already share a transcript, and both ends compute the same
//! handshake hash from it (proved in `noise_stream`'s tests), so a stream can
//! derive its own key from that root and start sending at once.
//!
//! Three properties are load-bearing and each has a test:
//!
//! - **A key belongs to one stream, one direction.** The derivation mixes the
//!   stream id and the direction, so two streams' keys differ even though they
//!   share a root, and the two directions of one stream differ too.
//! - **A packet number is a nonce, once.** The packet number comes from the
//!   carrier's own sequence, which never repeats within a connection, and a
//!   retransmission carries a **new** number — so a nonce is never reused with
//!   the same key.
//! - **The header is the associated data.** A packet's stream id, number and
//!   length are authenticated but not encrypted, so a packet cannot be moved to
//!   another stream, replayed under another number, or truncated.
//!
//! What this trades away is stated where the design is: forward secrecy moves
//! from per stream to per lane.

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

/// The tag every sealed packet carries.
pub(super) const TAG: usize = 16;

/// The label the derivation is bound to.
///
/// A version byte because the root is a long-lived value: changing what is
/// derived from it must not silently decrypt a newer stream's traffic as an
/// older one's.
const LABEL: &[u8] = b"molehill mhd stream v1";

/// Which way a stream's bytes are going.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    /// From the side that dialed the connection to the side that accepted it.
    ToServer,
    /// From the accepting side to the dialing side.
    ToClient,
}

impl Direction {
    fn bit(self) -> u8 {
        match self {
            Self::ToServer => 0,
            Self::ToClient => 1,
        }
    }
}

/// One direction of one stream.
pub(super) struct StreamCipher {
    aead: ChaCha20Poly1305,
}

impl std::fmt::Debug for StreamCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key: a debug print of a cipher is a key leak waiting for a
        // log line.
        f.write_str("StreamCipher(..)")
    }
}

impl StreamCipher {
    /// Derive the key for one direction of one stream.
    ///
    /// `root` is the lane's handshake hash — identical on both ends. `info`
    /// mixes the label, the direction and the stream id, so the derivation is
    /// injective over `(stream, direction)` for a given root.
    pub(super) fn derive(root: &[u8; 32], direction: Direction, stream: u32) -> Option<Self> {
        let mut info = Vec::with_capacity(LABEL.len() + 5);
        info.extend_from_slice(LABEL);
        info.push(direction.bit());
        info.extend_from_slice(&stream.to_be_bytes());

        let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(root), &[]);
        let mut key = [0u8; 32];
        // The only error `expand` reports is an output length beyond 255 hashes,
        // and 32 bytes is one block — but a refused derivation must not become a
        // panic in a data path, and a fallback key would be worse than no key.
        if hk.expand(&info, &mut key).is_err() {
            return None;
        }
        let aead = ChaCha20Poly1305::new(&Key::from(key));
        // The stack copy is wiped; the cipher keeps its own expanded schedule.
        key.fill(0);
        Some(Self { aead })
    }

    /// The nonce one packet uses: its own number, which the header carries.
    ///
    /// A real nonce rather than a counter this end keeps, because the counter
    /// *is* the packet number — which the carrier already guarantees is unique
    /// per connection and never reused for a retransmission. A local counter
    /// would have to stay in step with what the peer sees, and the first
    /// reordering bug would desynchronise it.
    fn nonce(packet: u64) -> Nonce {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&packet.to_be_bytes());
        Nonce::from(nonce)
    }

    /// Seal `plain` in place, answering the tag the caller appends.
    ///
    /// `ad` is the packet header, authenticated but not encrypted. The caller
    /// owns the layout — this writes nothing outside `plain` — which is what
    /// keeps the whole path `unsafe`-free.
    pub(super) fn seal(&self, packet: u64, ad: &[u8], plain: &mut [u8]) -> Option<[u8; TAG]> {
        let tag = self
            .aead
            .encrypt_inout_detached(&Self::nonce(packet), ad, plain.into())
            .ok()?;
        let mut out = [0u8; TAG];
        out.copy_from_slice(&tag);
        Some(out)
    }

    /// Open `cipher` in place against `tag`, answering whether it verified.
    pub(super) fn open(&self, packet: u64, ad: &[u8], cipher: &mut [u8], tag: &[u8]) -> bool {
        let Ok(tag) = <&[u8; TAG]>::try_from(tag) else {
            return false;
        };
        let tag = (*tag).into();
        self.aead
            .decrypt_inout_detached(&Self::nonce(packet), ad, cipher.into(), &tag)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    /// One round trip, and the tag is what a wrong anything fails on.
    #[test]
    fn a_packet_sealed_by_one_end_opens_on_the_other() {
        let Some(send) = StreamCipher::derive(&root(1), Direction::ToServer, 7) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let Some(recv) = StreamCipher::derive(&root(1), Direction::ToServer, 7) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let ad = b"header";
        let mut buf = *b"the quick brown fox";
        let Some(tag) = send.seal(42, ad, &mut buf) else {
            unreachable!("sealing into a sized buffer cannot fail")
        };
        assert!(recv.open(42, ad, &mut buf, &tag), "own tag opens");
        assert_eq!(&buf, b"the quick brown fox", "the plaintext comes back");
    }

    /// Two streams of one lane do not share a key, in either direction.
    #[test]
    fn every_stream_and_direction_has_its_own_key() {
        let root = root(2);
        let cases = [
            (Direction::ToServer, 1),
            (Direction::ToServer, 3),
            (Direction::ToClient, 1),
            (Direction::ToClient, 3),
        ];
        for (index, (direction, stream)) in cases.iter().enumerate() {
            let Some(send) = StreamCipher::derive(&root, *direction, *stream) else {
                unreachable!("a 32-byte derivation cannot fail")
            };
            // The sealed bytes, kept: `open` takes ciphertext, and re-sealing to
            // make a fresh one would test a different packet.
            let mut sealed = *b"payload";
            let Some(tag) = send.seal(1, b"h", &mut sealed) else {
                unreachable!("sealing into a sized buffer cannot fail")
            };
            for (other, (d, s)) in cases.iter().enumerate() {
                let mut buf = sealed;
                let Some(recv) = StreamCipher::derive(&root, *d, *s) else {
                    unreachable!("a 32-byte derivation cannot fail")
                };
                let opened = recv.open(1, b"h", &mut buf, &tag);
                assert_eq!(
                    opened,
                    index == other,
                    "{direction:?}/{stream} must not open {d:?}/{s}'s packet"
                );
                if opened {
                    assert_eq!(&buf, b"payload", "the plaintext must come back");
                }
            }
        }
    }

    /// A different lane root is a different key, even for the same stream.
    #[test]
    fn two_lanes_never_share_a_stream_key() {
        let Some(a) = StreamCipher::derive(&root(3), Direction::ToServer, 5) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let Some(b) = StreamCipher::derive(&root(4), Direction::ToServer, 5) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let mut buf = *b"payload";
        let Some(tag) = a.seal(1, b"h", &mut buf) else {
            unreachable!("sealing into a sized buffer cannot fail")
        };
        let mut buf = *b"payload";
        assert!(!b.open(1, b"h", &mut buf, &tag));
    }

    /// The header is authenticated: the same bytes under another number, or
    /// under another stream's header, do not open.
    #[test]
    fn the_header_is_authenticated() {
        let Some(send) = StreamCipher::derive(&root(5), Direction::ToServer, 9) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let Some(recv) = StreamCipher::derive(&root(5), Direction::ToServer, 9) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let mut buf = *b"payload";
        let Some(tag) = send.seal(10, b"stream=9 len=7", &mut buf) else {
            unreachable!("sealing into a sized buffer cannot fail")
        };
        let mut buf = *b"payload";
        assert!(
            !recv.open(11, b"stream=9 len=7", &mut buf, &tag),
            "another packet number must not open"
        );
        let mut buf = *b"payload";
        assert!(
            !recv.open(10, b"stream=11 len=7", &mut buf, &tag),
            "another header must not open"
        );
    }

    /// A flipped bit anywhere in the ciphertext, or in the tag, does not open.
    #[test]
    fn tampering_is_refused() {
        let Some(send) = StreamCipher::derive(&root(6), Direction::ToClient, 1) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let Some(recv) = StreamCipher::derive(&root(6), Direction::ToClient, 1) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let plain = *b"payload";
        let mut sealed = plain;
        let Some(tag) = send.seal(1, b"h", &mut sealed) else {
            unreachable!("sealing into a sized buffer cannot fail")
        };
        for byte in 0..plain.len() {
            let mut tampered = sealed;
            tampered[byte] ^= 0x01;
            assert!(!recv.open(1, b"h", &mut tampered, &tag), "byte {byte}");
        }
        for byte in 0..TAG {
            let mut bad = tag;
            bad[byte] ^= 0x01;
            let mut buf = sealed;
            assert!(!recv.open(1, b"h", &mut buf, &bad), "tag byte {byte}");
        }
    }

    /// A truncated tag is refused rather than read past.
    #[test]
    fn a_short_tag_is_refused() {
        let Some(send) = StreamCipher::derive(&root(7), Direction::ToServer, 1) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let Some(recv) = StreamCipher::derive(&root(7), Direction::ToServer, 1) else {
            unreachable!("a 32-byte derivation cannot fail")
        };
        let mut buf = *b"payload";
        let Some(tag) = send.seal(1, b"h", &mut buf) else {
            unreachable!("sealing into a sized buffer cannot fail")
        };
        let short = &tag[..TAG - 1];
        let mut buf = *b"payload";
        assert!(!recv.open(1, b"h", &mut buf, short));
    }
}
