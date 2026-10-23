//! The per-stream record layer: what data_enable means on this carrier.
//!
//! On the TCP carrier the switch wraps the connection's byte stream in the
//! Noise record stream, because one connection carries one byte stream. A
//! QUIC connection carries many streams, and snow's transport state cannot be
//! cloned per stream, so each stream derives its own key from the handshake
//! hash instead: HKDF-SHA256 with the stream id and the direction as the info
//! string. Framing is ours and deliberately small — a two-byte length, then
//! the ciphertext and its tag, with a 64-bit counter as the nonce.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;

/// The Poly1305 tag every frame carries.
pub(crate) const TAG_LEN: usize = 16;

/// The largest plaintext one frame may carry: the length prefix is a u16.
pub(crate) const MAX_PLAINTEXT: usize = u16::MAX as usize - TAG_LEN;

/// One direction of one stream.
pub(crate) struct RecordKey {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl RecordKey {
    /// Derive one direction's key for one stream.
    ///
    /// The handshake hash is the same secret both ends hold, and the info
    /// string separates the key by stream and by direction, so a frame cannot
    /// be replayed into another stream or reflected back at its sender. The
    /// flag names the *sender* of that direction, not the caller: both ends
    /// derive the same key for the same direction, and the opposite one for
    /// the other.
    pub(crate) fn derive(handshake_hash: &[u8], stream: u64, initiator: bool) -> Self {
        let hkdf = Hkdf::<Sha256>::new(None, handshake_hash);
        let mut info = Vec::with_capacity(16 + 8 + 1);
        info.extend_from_slice(b"molehill stream");
        info.extend_from_slice(&stream.to_be_bytes());
        info.push(u8::from(initiator));
        let mut key = [0u8; 32];
        hkdf.expand(&info, &mut key)
            .expect("32 bytes is a valid SHA-256 output length");
        RecordKey {
            cipher: ChaCha20Poly1305::new(&key.into()),
            counter: 0,
        }
    }

    fn nonce(&self) -> Nonce {
        let mut bytes = [0u8; 12];
        bytes[4..].copy_from_slice(&self.counter.to_be_bytes());
        Nonce::from(bytes)
    }

    /// Seal one frame: the returned buffer is len, ciphertext, tag.
    ///
    /// # Panics
    ///
    /// Panics when the plaintext is longer than MAX_PLAINTEXT; callers chunk
    /// their writes, so that is a bug rather than a network condition.
    pub(crate) fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        assert!(
            plaintext.len() <= MAX_PLAINTEXT,
            "a record frame cannot exceed {MAX_PLAINTEXT} bytes"
        );
        let ciphertext = self
            .cipher
            .encrypt(&self.nonce(), plaintext)
            .expect("ChaCha20-Poly1305 encryption cannot fail");
        self.counter += 1;
        let mut frame = Vec::with_capacity(2 + ciphertext.len());
        frame.extend_from_slice(&(ciphertext.len() as u16).to_be_bytes());
        frame.extend_from_slice(&ciphertext);
        frame
    }

    /// Open one frame, returning its plaintext when it authenticates.
    pub(crate) fn open(&mut self, frame: &[u8]) -> Option<Vec<u8>> {
        let (length, ciphertext) = frame.split_first_chunk::<2>()?;
        let length = usize::from(u16::from_be_bytes(*length));
        if ciphertext.len() != length {
            return None;
        }
        let plaintext = self.cipher.decrypt(&self.nonce(), ciphertext).ok()?;
        self.counter += 1;
        Some(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: [u8; 32] = [7u8; 32];

    #[test]
    fn a_frame_round_trips() {
        let mut writer = RecordKey::derive(&HASH, 0, true);
        let mut reader = RecordKey::derive(&HASH, 0, true);
        let frame = writer.seal(b"the visitor's bytes");
        assert_eq!(frame.len(), 2 + 19 + TAG_LEN);
        assert_eq!(
            reader.open(&frame).as_deref(),
            Some(&b"the visitor's bytes"[..])
        );
    }

    #[test]
    fn the_counter_makes_every_frame_different() {
        let mut key = RecordKey::derive(&HASH, 0, true);
        let first = key.seal(b"same bytes");
        let second = key.seal(b"same bytes");
        assert_ne!(first, second, "a repeated nonce would be a break");
    }

    #[test]
    fn another_stream_cannot_open_the_frame() {
        let mut writer = RecordKey::derive(&HASH, 1, true);
        let mut other = RecordKey::derive(&HASH, 0, true);
        let frame = writer.seal(b"stream one");
        assert!(other.open(&frame).is_none());
    }

    #[test]
    fn a_tampered_frame_is_refused() {
        let mut writer = RecordKey::derive(&HASH, 0, true);
        let mut reader = RecordKey::derive(&HASH, 0, true);
        let mut frame = writer.seal(b"the visitor's bytes");
        let last = frame.len() - 1;
        frame[last] ^= 0x80;
        assert!(reader.open(&frame).is_none());
    }

    #[test]
    fn a_frame_whose_length_disagrees_is_refused() {
        let mut writer = RecordKey::derive(&HASH, 0, true);
        let mut reader = RecordKey::derive(&HASH, 0, true);
        let mut frame = writer.seal(b"short");
        frame[1] += 1;
        assert!(reader.open(&frame).is_none());
    }
}
