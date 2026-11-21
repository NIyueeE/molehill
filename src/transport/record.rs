//! The record layer: what seals and opens every `u16`-framed record.
//!
//! The Noise KK handshake stays exactly as it is — it is what authenticates the
//! two ends and what gives the session its secrets — but the **record cipher is
//! ours**, keyed from that handshake: `AES-256-GCM` from `ring`, with keys
//! derived through HKDF from the handshake's raw split salted with its
//! transcript hash.
//!
//! ## Why not the handshake's own cipher
//!
//! The KK pattern names `ChaChaPoly`, and with `snow`'s `ring-accelerated`
//! feature that is `ring`'s implementation: a release build sampled under
//! `perf` on the bulk path spent **17.4 %** of its cycles in
//! `chacha20_poly1305_open_avx2` and **15.4 %** in `..._seal_avx2` — a third of
//! the process on one primitive. At the size this layer actually seals,
//! `ring`'s `AES-256-GCM` takes **12.2 µs** per 32 KiB where the same crate's
//! ChaCha20-Poly1305 takes **17.3 µs** (0.371 against 0.526 ns per byte;
//! `tests/primitives_test.rs` prices both), and AES-GCM is what the mhd lane
//! already seals under.
//!
//! Keeping the handshake and changing only the record cipher is deliberate: the
//! handshake is diffie-hellman plus authentication and runs once per session,
//! so nothing measured rides on its cipher, while the record cipher runs per
//! 64 KiB and nothing else in the data path costs as much per byte.
//!
//! ## What the derivation guarantees
//!
//! - **A key is one direction of one session.** HKDF's input key material is
//!   the direction's raw split half; the salt is the handshake hash; the info
//!   names the label and the direction. Both ends derive the same pair and pick
//!   opposite halves.
//! - **The transcript binds it.** The split alone is secret but says nothing
//!   about which handshake it came from; salting with the transcript hash binds
//!   every record key to this exchange, exactly as `mhd::cipher::LaneRoot` does
//!   for a lane's streams.
//! - **A record is sealed once.** The nonce is a per-direction counter in a
//!   96-bit field, incremented per record and never reused: the transport is a
//!   byte stream, so the two ends see the records in the same order. A
//!   `u64` of records is more than any session can move; reaching it is an error, not
//!   a wrap.
//! - **The header is authenticated.** The record's two-byte length field is
//!   the associated data, so a record cannot be re-framed or truncated — a
//!   property the handshake's own transport mode does not have, because it
//!   carries no associated data at all.
//!
//! The wire format is unchanged (`u16` ciphertext length, then ciphertext, then
//! a 16-byte tag) apart from the cipher that produces it, which is a change to
//! the dialect and rides the unreleased 6.0.

use std::io::{Error, ErrorKind};

use hkdf::Hkdf;
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

/// The tag every record carries.
pub(crate) const TAG: usize = 16;

/// The record's cleartext length field: the ciphertext length, tag included.
pub(crate) const LENGTH_FIELD: usize = std::mem::size_of::<u16>();

/// Which side of the handshake this end ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The side that dialed; its sends use the handshake's first split half.
    Initiator,
    /// The side that accepted; its sends use the second.
    Responder,
}

impl Role {
    /// The split half this end seals with: Noise splits
    /// `(initiator → responder, responder → initiator)` in that order.
    fn send_half(self) -> usize {
        match self {
            Self::Initiator => 0,
            Self::Responder => 1,
        }
    }
}

/// The label the derivation is bound to, with a version byte: the wire dialect
/// may change what a record key is derived from, and a newer peer's records
/// must not open as an older peer's.
const LABEL: &[u8] = b"molehill noise record v1";

/// One session's two directions.
pub(crate) struct RecordCipher {
    send: LessSafeKey,
    recv: LessSafeKey,
    sent: u64,
    received: u64,
}

impl std::fmt::Debug for RecordCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the key: a debug print of a cipher is a key leak waiting for a
        // log line.
        f.write_str("RecordCipher(..)")
    }
}

impl RecordCipher {
    /// Derive this session's pair from one finished handshake.
    ///
    /// `split` is the handshake's two directional secret keys (snow's
    /// `dangerously_get_raw_split`) and `transcript` its handshake hash. `None`
    /// means "do not derive" — a peer must then fail the session rather than
    /// seal under a key nobody computed.
    pub(crate) fn from_handshake(
        split: &[[u8; 32]; 2],
        transcript: &[u8; 32],
        role: Role,
    ) -> Option<Self> {
        let send = derive(&split[role.send_half()], transcript, role.send_half())?;
        let recv = derive(
            &split[1 - role.send_half()],
            transcript,
            1 - role.send_half(),
        )?;
        let key = |bytes: &[u8; 32]| {
            UnboundKey::new(&AES_256_GCM, bytes)
                .ok()
                .map(LessSafeKey::new)
        };
        let pair = key(&send).zip(key(&recv));
        pair.map(|(send, recv)| Self {
            send,
            recv,
            sent: 0,
            received: 0,
        })
    }

    /// Seal one record: `plain` into `out` behind the length header, tag
    /// appended.
    ///
    /// `out` must be at least `LENGTH_FIELD + plain.len() + TAG` bytes. Answers
    /// the record's total wire length.
    pub(crate) fn seal(&mut self, plain: &[u8], out: &mut [u8]) -> Result<usize, Error> {
        let body = plain.len() + TAG;
        let total = LENGTH_FIELD + body;
        if out.len() < total {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "record buffer too small for a sealed record",
            ));
        }
        let len = u16::try_from(body).map_err(|_| {
            Error::new(
                ErrorKind::InvalidInput,
                "noise record length exceeds the u16 wire format",
            )
        })?;
        // The header is the associated data, so it is written before the
        // seal and authenticated by it.
        out[..LENGTH_FIELD].copy_from_slice(&len.to_le_bytes());
        let (header, rest) = out[..total].split_at_mut(LENGTH_FIELD);
        let (region, tag_out) = rest.split_at_mut(plain.len());
        region.copy_from_slice(plain);

        let nonce = self.next_send()?;
        let tag = self
            .send
            .seal_in_place_separate_tag(nonce, Aad::from(&*header), region)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "noise record seal failed"))?;
        tag_out[..TAG].copy_from_slice(tag.as_ref());
        Ok(total)
    }

    /// Open one record in place: `record` is the header, the ciphertext and the
    /// tag, and the plaintext is written over the ciphertext.
    ///
    /// Answers the plaintext length; the plaintext is
    /// `record[LENGTH_FIELD..LENGTH_FIELD + len]`.
    pub(crate) fn open(&mut self, record: &mut [u8]) -> Result<usize, Error> {
        if record.len() < LENGTH_FIELD + TAG {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "noise record shorter than a tag",
            ));
        }
        let declared = usize::from(u16::from_le_bytes([record[0], record[1]]));
        if declared != record.len() - LENGTH_FIELD {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "noise record length field disagrees with the bytes read",
            ));
        }
        let (header, rest) = record.split_at_mut(LENGTH_FIELD);
        let nonce = self.next_receive()?;
        let plain = self
            .recv
            .open_in_place(nonce, Aad::from(&*header), rest)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "noise record failed to open"))?;
        Ok(plain.len())
    }

    /// The next send nonce, or an error at the end of the counter.
    fn next_send(&mut self) -> Result<Nonce, Error> {
        let nonce = nonce_of(self.sent);
        self.sent = self.sent.checked_add(1).ok_or_else(exhausted)?;
        Ok(nonce)
    }

    /// The next receive nonce, or an error at the end of the counter.
    fn next_receive(&mut self) -> Result<Nonce, Error> {
        let nonce = nonce_of(self.received);
        self.received = self.received.checked_add(1).ok_or_else(exhausted)?;
        Ok(nonce)
    }
}

/// One direction's key: HKDF over the split half, salted with the transcript.
///
/// `direction` is part of the info as well as selecting the input, so the two
/// directions are separated twice over — a caller that mixed the halves up
/// still cannot open the other direction's records.
fn derive(half: &[u8; 32], transcript: &[u8; 32], direction: usize) -> Option<[u8; 32]> {
    let mut info = [0u8; LABEL.len() + 1];
    info[..LABEL.len()].copy_from_slice(LABEL);
    info[LABEL.len()] = u8::try_from(direction).ok()?;
    let hk = Hkdf::<sha2::Sha256>::new(Some(transcript), half);
    let mut key = [0u8; 32];
    // The only error `expand` reports is an output longer than 255 hashes, and
    // 32 bytes is one block — but a refused derivation must not become a panic
    // in a data path.
    let derived = hk.expand(&info, &mut key).is_ok();
    if !derived {
        key.fill(0);
        return None;
    }
    Some(key)
}

/// The nonce a counter value names: a 96-bit field, the counter in its last
/// eight bytes.
fn nonce_of(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_be_bytes());
    Nonce::assume_unique_for_key(nonce)
}

/// The error a session reports when its record counter runs out.
fn exhausted() -> Error {
    Error::new(
        ErrorKind::InvalidData,
        "noise record counter exhausted; the session must be re-established",
    )
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::expect_used,
        reason = "tests expect on values they just constructed"
    )]
    use super::*;

    /// A handshake's outputs, as both ends see them.
    fn handshake(seed: u8) -> ([[u8; 32]; 2], [u8; 32]) {
        ([[seed; 32], [seed.wrapping_add(1); 32]], [seed; 32])
    }

    /// A pair, sealed on one side and opened on the other, per direction.
    #[test]
    fn a_record_sealed_by_one_end_opens_on_the_other() {
        let (split, transcript) = handshake(9);
        let mut init = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let mut resp = RecordCipher::from_handshake(&split, &transcript, Role::Responder)
            .expect("a 32-byte derivation cannot fail");

        let mut wire = [0u8; LENGTH_FIELD + 5 + TAG];
        let total = init
            .seal(b"hello", &mut wire)
            .expect("seals into a sized buffer");
        assert_eq!(
            total,
            wire.len(),
            "the record is header, ciphertext and tag"
        );
        assert_eq!(
            &wire[..LENGTH_FIELD],
            &(5u16 + u16::try_from(TAG).expect("a 16-byte tag fits")).to_le_bytes(),
            "the length field counts the tag"
        );
        assert_ne!(
            &wire[LENGTH_FIELD..LENGTH_FIELD + 5],
            b"hello",
            "the body is sealed"
        );
        let n = resp.open(&mut wire[..total]).expect("the peer opens it");
        assert_eq!(&wire[LENGTH_FIELD..LENGTH_FIELD + n], b"hello");

        // And the other direction, which is a different key.
        let total = resp
            .seal(b"world", &mut wire)
            .expect("seals into a sized buffer");
        let n = init.open(&mut wire[..total]).expect("the peer opens it");
        assert_eq!(&wire[LENGTH_FIELD..LENGTH_FIELD + n], b"world");
    }

    /// Two records of the same plaintext are different bytes: the nonce moves.
    #[test]
    fn every_record_gets_its_own_nonce() {
        let (split, transcript) = handshake(11);
        let mut send = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let mut first = [0u8; LENGTH_FIELD + 8 + TAG];
        let mut second = [0u8; LENGTH_FIELD + 8 + TAG];
        send.seal(b"same one", &mut first).expect("seals");
        send.seal(b"same one", &mut second).expect("seals");
        assert_ne!(
            first, second,
            "a repeated nonce would repeat the ciphertext"
        );
    }

    /// A record the counter has moved past does not open: the nonce is the
    /// record's place in the stream, not a field it carries.
    #[test]
    fn a_skipped_record_breaks_the_counter() {
        let (split, transcript) = handshake(13);
        let mut send = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let mut recv = RecordCipher::from_handshake(&split, &transcript, Role::Responder)
            .expect("a 32-byte derivation cannot fail");
        let mut skipped = [0u8; LENGTH_FIELD + 4 + TAG];
        let mut next = [0u8; LENGTH_FIELD + 4 + TAG];
        send.seal(b"lost", &mut skipped).expect("seals");
        send.seal(b"kept", &mut next).expect("seals");
        assert!(
            recv.open(&mut next).is_err(),
            "the receiver is one nonce behind and must refuse"
        );
    }

    /// Altering any part of the record fails to open it.
    #[test]
    fn a_tampered_record_does_not_open() {
        let (split, transcript) = handshake(17);
        let mut send = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let sealed = {
            let mut wire = [0u8; LENGTH_FIELD + 6 + TAG];
            let total = send.seal(b"secret", &mut wire).expect("seals");
            wire[..total].to_vec()
        };
        for at in [0usize, LENGTH_FIELD, sealed.len() - 1] {
            let mut wire = sealed.clone();
            wire[at] ^= 0x01;
            let mut recv = RecordCipher::from_handshake(&split, &transcript, Role::Responder)
                .expect("a 32-byte derivation cannot fail");
            assert!(
                recv.open(&mut wire).is_err(),
                "a flipped byte at {at} must not open"
            );
        }
    }

    /// The two ends' keys are not interchangeable: a side cannot open what it
    /// sealed.
    #[test]
    fn a_direction_does_not_open_itself() {
        let (split, transcript) = handshake(19);
        let mut init = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let mut wire = [0u8; LENGTH_FIELD + 3 + TAG];
        let total = init.seal(b"abc", &mut wire).expect("seals");
        assert!(init.open(&mut wire[..total]).is_err());
    }

    /// A different transcript is a different key: two sessions never share one.
    #[test]
    fn two_sessions_never_share_a_key() {
        let (split, transcript) = handshake(23);
        let mut other = transcript;
        other[0] ^= 0xff;
        let mut send = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let mut recv = RecordCipher::from_handshake(&split, &other, Role::Responder)
            .expect("a 32-byte derivation cannot fail");
        let mut wire = [0u8; LENGTH_FIELD + 3 + TAG];
        let total = send.seal(b"abc", &mut wire).expect("seals");
        assert!(recv.open(&mut wire[..total]).is_err());
    }

    /// A length field the record does not match is refused before the cipher
    /// runs, so the framing cannot be re-cut.
    #[test]
    fn a_reframed_record_is_refused() {
        let (split, transcript) = handshake(29);
        let mut send = RecordCipher::from_handshake(&split, &transcript, Role::Initiator)
            .expect("a 32-byte derivation cannot fail");
        let mut recv = RecordCipher::from_handshake(&split, &transcript, Role::Responder)
            .expect("a 32-byte derivation cannot fail");
        let mut wire = [0u8; LENGTH_FIELD + 4 + TAG];
        send.seal(b"four", &mut wire).expect("seals");
        let mut short = wire;
        let len = short.len();
        assert!(
            recv.open(&mut short[..len - 1]).is_err(),
            "a truncated record"
        );
    }
}
