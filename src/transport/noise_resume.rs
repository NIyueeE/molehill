//! Noise session resume: a reconnect that skips the handshake's DH turns.
//!
//! A full Noise handshake with the production pattern
//! (`Noise_NK_25519_ChaChaPoly_BLAKE2s`) spends the overwhelming majority
//! of its CPU in the key exchanges — a release-mode phase attribution
//! (`noise_stream.rs`, `handshake_phase_attribution`) measures the DH
//! turns at ~97% of the ~445 us a pair costs on the state machine. This
//! module replaces them on a reconnect: a client that has completed a
//! handshake once proves it still holds that session's handshake hash
//! with a MAC, and both sides derive fresh record keys from the cached
//! hash plus two fresh nonces.
//!
//! ```text
//! full connection (selector 0x01)                 resumed connection (0x02)
//!   ═══ NK handshake (DH ×3) ═══>                   ticket || nonce || MAC ──>
//!   <══ ticket, sealed for the server ═══           <══ nonce || MAC (verdict)
//!   ═════ record cipher from the DHs ════           ═════ record cipher from
//!                                                    HKDF(cached hash, nonces)
//! ```
//!
//! Security posture, stated plainly:
//!
//! - The ticket is sealed with a key derived from the server's Noise
//!   static private key, so only that server can open it; the client
//!   carries it opaquely. The client's MAC proves possession of the
//!   cached hash — a stolen ticket alone can be *offered*, never
//!   completed.
//! - Fresh record keys per resumed connection (HKDF over the cached hash
//!   with both nonces), and the server rejects a repeated
//!   `(ticket, client nonce)` pair, so a replayed request cannot make
//!   two sessions share a key.
//! - Forward secrecy: the original session keeps its DH-derived keys. A
//!   resumed session's keys derive from the cached hash without a fresh
//!   DH, so a later compromise of the server static key (or of the
//!   client's cache) reaches the resumed sessions' traffic. This is the
//!   standard session-resumption tradeoff; operators who need
//!   per-connection forward secrecy keep resume disabled (the default).
//! - Every failure (unknown, stale, or tampered ticket; bad MAC; replay)
//!   makes the responder decline, and the initiator falls back to a full
//!   handshake on a fresh connection.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand::TryRng;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

/// Transport selector for a resumed Noise connection (the plain/full
/// handshake selectors are `0x00`/`0x01`).
pub const NOISE_RESUME_SELECTOR: u8 = 0x02;

/// Domain-separation label for every derivation in this module.
const RESUME_LABEL: &[u8] = b"molehill-noise-resume-v1";
/// Length of the random nonces each side contributes to a resumed
/// session's key derivation.
const NONCE_LEN: usize = 32;
/// Length of a MAC proving knowledge of the cached handshake hash.
const MAC_LEN: usize = 32;
/// Length of the sealed ticket: AEAD nonce, then the sealed
/// `[id 8][issued 8][handshake hash 32]` plus its tag.
const TICKET_PLAIN_LEN: usize = 8 + 8 + 32;
const TICKET_LEN: usize = 12 + TICKET_PLAIN_LEN + 16;
/// Entries each side may hold before the cache is cleared wholesale.
/// Each entry is ~100 bytes, so the bound is memory-trivial.
const CACHE_CAP: usize = 4096;
/// How long a ticket stays valid; reconnects after this window fall back
/// to a full handshake.
const TICKET_TTL_SECS: u64 = 24 * 60 * 60;
/// Status byte of the responder's verdict.
const VERDICT_RESUMED: u8 = 1;
const VERDICT_DECLINED: u8 = 0;
/// AEAD tag length.
const TAG_LEN: usize = 16;

// ---------------------------------------------------------------- crypto

/// HMAC-SHA256 (RFC 2104) over the crate's `sha2` dependency.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; MAC_LEN] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    for b in &key_block {
        inner.update([b ^ 0x36]);
    }
    inner.update(msg);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    for b in &key_block {
        outer.update([b ^ 0x5c]);
    }
    outer.update(inner);
    outer.finalize().into()
}

/// HKDF-SHA256 (RFC 5869): extract-then-expand into `out`.
fn hkdf_sha256(ikm: &[u8], salt: &[u8], info: &[u8], out: &mut [u8]) {
    let prk = hmac_sha256(salt, ikm);
    let mut block: Vec<u8> = Vec::with_capacity(out.len() + 32);
    let mut counter: u8 = 1;
    while block.len() < out.len() {
        assert!(out.len() <= 255 * 32, "HKDF output too large");
        let mut input = Vec::with_capacity(32 + info.len() + 1);
        input.extend_from_slice(&block[block.len().saturating_sub(32)..]);
        input.extend_from_slice(info);
        input.push(counter);
        let t = hmac_sha256(&prk, &input);
        block.extend_from_slice(&t);
        counter = counter.wrapping_add(1);
    }
    out.copy_from_slice(&block[..out.len()]);
}

/// Derive one role-labelled key from a cached handshake hash.
fn role_key(hh: &[u8], role: &[u8]) -> [u8; 32] {
    let mut key = [0u8; 32];
    hkdf_sha256(hh, RESUME_LABEL, role, &mut key);
    key
}

/// The two record keys of a resumed session.
#[derive(Clone)]
struct SessionKeys {
    initiator_egress: [u8; 32],
    responder_egress: [u8; 32],
}

impl SessionKeys {
    fn derive(hh: &[u8], client_nonce: &[u8; NONCE_LEN], server_nonce: &[u8; NONCE_LEN]) -> Self {
        let mut init_info = Vec::with_capacity(4 + 2 * NONCE_LEN);
        init_info.extend_from_slice(b"init");
        init_info.extend_from_slice(client_nonce);
        init_info.extend_from_slice(server_nonce);
        let mut resp_info = Vec::with_capacity(4 + 2 * NONCE_LEN);
        resp_info.extend_from_slice(b"resp");
        resp_info.extend_from_slice(client_nonce);
        resp_info.extend_from_slice(server_nonce);
        let mut keys = SessionKeys {
            initiator_egress: [0u8; 32],
            responder_egress: [0u8; 32],
        };
        hkdf_sha256(hh, RESUME_LABEL, &init_info, &mut keys.initiator_egress);
        hkdf_sha256(hh, RESUME_LABEL, &resp_info, &mut keys.responder_egress);
        keys
    }
}

/// The record cipher of a resumed session: one AEAD key per direction,
/// each with its own nonce counter starting at zero.
///
/// The zero start is safe because the keys are fresh per connection.
pub struct ResumedCipher {
    send: ChaCha20Poly1305,
    recv: ChaCha20Poly1305,
    send_nonce: u64,
    recv_nonce: u64,
    /// Staging buffer for the in-place decrypt: the AEAD appends/removes
    /// its tag in place, while the record layer's callers hand us exactly
    /// the ciphertext and expect the plaintext back. `snow`'s stateful
    /// transport hides this; the stateful construction cannot.
    scratch: Vec<u8>,
}

impl ResumedCipher {
    /// The initiator's view: it sends with the initiator-egress key.
    pub fn initiator(
        hh: &[u8],
        client_nonce: &[u8; NONCE_LEN],
        server_nonce: &[u8; NONCE_LEN],
    ) -> Self {
        let keys = SessionKeys::derive(hh, client_nonce, server_nonce);
        ResumedCipher {
            send: ChaCha20Poly1305::new(Key::from_slice(&keys.initiator_egress)),
            recv: ChaCha20Poly1305::new(Key::from_slice(&keys.responder_egress)),
            send_nonce: 0,
            recv_nonce: 0,
            scratch: Vec::new(),
        }
    }

    /// The responder's view: it sends with the responder-egress key.
    pub fn responder(
        hh: &[u8],
        client_nonce: &[u8; NONCE_LEN],
        server_nonce: &[u8; NONCE_LEN],
    ) -> Self {
        let keys = SessionKeys::derive(hh, client_nonce, server_nonce);
        ResumedCipher {
            send: ChaCha20Poly1305::new(Key::from_slice(&keys.responder_egress)),
            recv: ChaCha20Poly1305::new(Key::from_slice(&keys.initiator_egress)),
            send_nonce: 0,
            recv_nonce: 0,
            scratch: Vec::new(),
        }
    }

    /// Encrypt one record into `out` (sized `plaintext.len() + 16`),
    /// returning the ciphertext length.
    pub fn encrypt(&mut self, plaintext: &[u8], out: &mut [u8]) -> Result<usize, String> {
        let end = plaintext.len() + TAG_LEN;
        if out.len() < end {
            return Err("record buffer too small for the tag".to_owned());
        }
        out[..plaintext.len()].copy_from_slice(plaintext);
        let mut buf = SliceBuffer::new(out, plaintext.len());
        self.send
            .encrypt_in_place(&record_nonce(self.send_nonce), b"", &mut buf)
            .map_err(|e| e.to_string())?;
        self.send_nonce = self.send_nonce.wrapping_add(1);
        Ok(plaintext.len() + TAG_LEN)
    }

    /// Decrypt one record into `out`, returning the plaintext length.
    pub fn decrypt(&mut self, ciphertext: &[u8], out: &mut [u8]) -> Result<usize, String> {
        if ciphertext.len() < TAG_LEN || out.len() < ciphertext.len() - TAG_LEN {
            return Err("record buffer too small".to_owned());
        }
        let plain_len = ciphertext.len() - TAG_LEN;
        if self.scratch.len() < ciphertext.len() {
            // Grow to at least a record's worth, so the amortized cost is
            // one resize per connection rather than per record.
            self.scratch.resize(ciphertext.len().next_power_of_two(), 0);
        }
        let staged = &mut self.scratch[..ciphertext.len()];
        staged.copy_from_slice(ciphertext);
        let mut buf = SliceBuffer::new(staged, ciphertext.len());
        self.recv
            .decrypt_in_place(&record_nonce(self.recv_nonce), b"", &mut buf)
            .map_err(|e| e.to_string())?;
        self.recv_nonce = self.recv_nonce.wrapping_add(1);
        out[..plain_len].copy_from_slice(&self.scratch[..plain_len]);
        Ok(plain_len)
    }
}

/// A fixed-capacity `aead::Buffer` view over a caller slice: the tag is
/// appended into the slice's spare capacity, so the record path keeps
/// its zero-allocation buffers (`aead` implements `Buffer` for `Vec`
/// only, and the record stream writes into a pooled array).
struct SliceBuffer<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl SliceBuffer<'_> {
    fn new(buf: &mut [u8], len: usize) -> SliceBuffer<'_> {
        SliceBuffer { buf, len }
    }
}

impl AsRef<[u8]> for SliceBuffer<'_> {
    fn as_ref(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl AsMut<[u8]> for SliceBuffer<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf[..self.len]
    }
}

impl chacha20poly1305::aead::Buffer for SliceBuffer<'_> {
    fn extend_from_slice(&mut self, other: &[u8]) -> Result<(), chacha20poly1305::aead::Error> {
        let end = self.len + other.len();
        if end > self.buf.len() {
            return Err(chacha20poly1305::aead::Error);
        }
        self.buf[self.len..end].copy_from_slice(other);
        self.len = end;
        Ok(())
    }

    fn truncate(&mut self, len: usize) {
        self.len = len.min(self.buf.len());
    }
}

/// The 12-byte record nonce: Noise's u64 little-endian counter
/// convention, zero-padded.
fn record_nonce(counter: u64) -> Nonce {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&nonce)
}

/// A fresh random nonce/identifier from the system RNG.
///
/// A failure is propagated, exactly like the crate's other RNG call sites
/// (`handshake_control_channel`, the KCP conversation id): a missing
/// secure nonce must fail the connection, not panic the process.
fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut out = [0u8; N];
    rand::rngs::SysRng
        .try_fill_bytes(&mut out)
        .map_err(|e| std::io::Error::other(format!("system rng: {e}")))?;
    Ok(out)
}

/// The current unix time in seconds, or 0 if the clock is behind the
/// epoch (a ticket issued then is simply rejected as stale).
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

// ---------------------------------------------------------------- tickets

/// Seal a ticket: the AEAD covers the ticket id, the issue time and the
/// handshake hash under the server's ticket key.
fn seal_ticket(
    ticket_key: &[u8; 32],
    id: u64,
    issued: u64,
    hh: &[u8; 32],
) -> std::io::Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(ticket_key));
    let nonce_bytes = random_bytes::<12>()?;
    let mut plain = Vec::with_capacity(TICKET_PLAIN_LEN);
    plain.extend_from_slice(&id.to_be_bytes());
    plain.extend_from_slice(&issued.to_be_bytes());
    plain.extend_from_slice(hh);
    // `Vec`'s `Buffer` impl appends the tag, so the sealed plaintext is
    // `TICKET_PLAIN_LEN + TAG_LEN` and the ticket is exactly
    // `TICKET_LEN`.
    let mut sealed = plain;
    cipher
        .encrypt_in_place(Nonce::from_slice(&nonce_bytes), b"", &mut sealed)
        .map_err(|_| std::io::Error::other("ticket seal failed"))?;
    debug_assert_eq!(sealed.len(), TICKET_PLAIN_LEN + TAG_LEN);
    let mut ticket = Vec::with_capacity(TICKET_LEN);
    ticket.extend_from_slice(&nonce_bytes);
    ticket.extend_from_slice(&sealed);
    Ok(ticket)
}

/// Open a sealed ticket, returning `(id, issued, handshake hash)`.
/// Tampering, or a ticket from a different server key, fails here.
fn open_ticket(ticket_key: &[u8; 32], ticket: &[u8]) -> Option<(u64, u64, [u8; 32])> {
    if ticket.len() != TICKET_LEN {
        return None;
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(ticket_key));
    let mut buf = ticket[12..].to_vec();
    cipher
        .decrypt_in_place(Nonce::from_slice(&ticket[..12]), b"", &mut buf)
        .ok()?;
    if buf.len() != TICKET_PLAIN_LEN {
        return None;
    }
    let id = u64::from_be_bytes(buf[..8].try_into().ok()?);
    let issued = u64::from_be_bytes(buf[8..16].try_into().ok()?);
    let mut hh = [0u8; 32];
    hh.copy_from_slice(&buf[16..]);
    Some((id, issued, hh))
}

// ---------------------------------------------------------------- caches

/// The initiator's resume cache: one entry per server static key, so a
/// client that talks to several servers resumes each independently.
#[derive(Default)]
pub struct ClientResumeCache {
    entries: Mutex<HashMap<[u8; 32], ResumeEntry>>,
}

struct ResumeEntry {
    ticket: Vec<u8>,
    handshake_hash: [u8; 32],
}

impl ClientResumeCache {
    fn store(&self, server_static: &[u8], ticket: Vec<u8>, handshake_hash: [u8; 32]) {
        let Ok(key) = <[u8; 32]>::try_from(server_static) else {
            return; // no usable static key: nothing to key the entry on
        };
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.len() >= CACHE_CAP && !entries.contains_key(&key) {
            entries.clear(); // bounded: this is a cache, not a store
        }
        entries.insert(
            key,
            ResumeEntry {
                ticket,
                handshake_hash,
            },
        );
    }

    fn take(&self, server_static: &[u8]) -> Option<(Vec<u8>, [u8; 32])> {
        let key = <[u8; 32]>::try_from(server_static).ok()?;
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        entries
            .get(&key)
            .map(|e| (e.ticket.clone(), e.handshake_hash))
    }

    fn drop_entry(&self, server_static: &[u8]) {
        if let Ok(key) = <[u8; 32]>::try_from(server_static) {
            self.entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
        }
    }
}

/// The responder's store: the tickets it can still open, plus the client
/// nonce each was last used with (the replay guard).
#[derive(Default)]
pub struct ServerResumeStore {
    ticket_key: [u8; 32],
    entries: Mutex<HashMap<u64, ServerEntry>>,
}

struct ServerEntry {
    handshake_hash: [u8; 32],
    issued: u64,
    last_client_nonce: Option<[u8; NONCE_LEN]>,
}

impl ServerResumeStore {
    /// Derive the ticket key from the responder's Noise static private
    /// key. With an ephemeral key the store dies with the process — old
    /// tickets simply stop opening, and clients fall back.
    pub fn new(local_private_key: &[u8]) -> Self {
        let mut ticket_key = [0u8; 32];
        hkdf_sha256(local_private_key, RESUME_LABEL, b"ticket", &mut ticket_key);
        ServerResumeStore {
            ticket_key,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Seal and remember a fresh ticket for a completed handshake.
    fn issue(&self, hh: &[u8]) -> std::io::Result<Vec<u8>> {
        let id = u64::from_be_bytes(random_bytes::<8>()?);
        let issued = unix_now();
        let mut hh_arr = [0u8; 32];
        let n = hh.len().min(32);
        hh_arr[..n].copy_from_slice(&hh[..n]);
        let ticket = seal_ticket(&self.ticket_key, id, issued, &hh_arr)?;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.len() >= CACHE_CAP {
            entries.clear();
        }
        entries.insert(
            id,
            ServerEntry {
                handshake_hash: hh_arr,
                issued,
                last_client_nonce: None,
            },
        );
        Ok(ticket)
    }

    /// Verify a resume request: open the ticket, check its age and the
    /// client's MAC, and reserve the client nonce (the replay guard).
    pub fn verify(
        &self,
        ticket: &[u8],
        client_nonce: &[u8; NONCE_LEN],
        mac: &[u8; MAC_LEN],
    ) -> Option<[u8; 32]> {
        let (id, issued, hh) = open_ticket(&self.ticket_key, ticket)?;
        if unix_now().saturating_sub(issued) > TICKET_TTL_SECS {
            debug!("resume ticket is older than {TICKET_TTL_SECS}s");
            return None;
        }
        let client_mac_key = role_key(&hh, b"client-mac");
        let mut mac_input = Vec::with_capacity(9 + ticket.len() + NONCE_LEN);
        mac_input.extend_from_slice(b"client-req");
        mac_input.extend_from_slice(ticket);
        mac_input.extend_from_slice(client_nonce);
        if hmac_sha256(&client_mac_key, &mac_input) != *mac {
            debug!("resume request carried a wrong client MAC");
            return None;
        }
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = entries.get_mut(&id)?;
        if entry.handshake_hash != hh {
            return None; // the seal authenticated, but the store disagrees
        }
        // The store's own timestamp is the TTL authority (the sealed one
        // was checked above; the two agree by construction).
        if unix_now().saturating_sub(entry.issued) > TICKET_TTL_SECS {
            debug!("resume ticket is past the store's TTL");
            return None;
        }
        if entry.last_client_nonce.as_ref() == Some(client_nonce) {
            debug!("resume ticket replayed with the same client nonce");
            return None;
        }
        entry.last_client_nonce = Some(*client_nonce);
        Some(hh)
    }
}

// ---------------------------------------------------------------- messages

/// The initiator's resume request: the cached ticket, a fresh client
/// nonce, and the MAC proving the cached handshake hash.
#[derive(Clone)]
pub struct ResumeRequest {
    server_static: Vec<u8>,
    ticket: Vec<u8>,
    handshake_hash: [u8; 32],
    client_nonce: [u8; NONCE_LEN],
    /// Present once the request was read off a stream; the responder
    /// checks it (an outbound request computes its MAC on encode).
    mac: Option<[u8; MAC_LEN]>,
}

impl ResumeRequest {
    /// Build a request from the client cache, or `Ok(None)` when there is
    /// no cached session for this server static key.
    pub fn build(cache: &ClientResumeCache, server_static: &[u8]) -> std::io::Result<Option<Self>> {
        let Some((ticket, handshake_hash)) = cache.take(server_static) else {
            return Ok(None);
        };
        Ok(Some(ResumeRequest {
            server_static: server_static.to_vec(),
            ticket,
            handshake_hash,
            client_nonce: random_bytes::<NONCE_LEN>()?,
            mac: None,
        }))
    }

    fn mac(&self) -> [u8; MAC_LEN] {
        let client_mac_key = role_key(&self.handshake_hash, b"client-mac");
        let mut mac_input = Vec::with_capacity(9 + self.ticket.len() + NONCE_LEN);
        mac_input.extend_from_slice(b"client-req");
        mac_input.extend_from_slice(&self.ticket);
        mac_input.extend_from_slice(&self.client_nonce);
        hmac_sha256(&client_mac_key, &mac_input)
    }

    /// Serialize the request: `[u16 ticket_len][ticket][nonce][mac]`.
    fn encode(&self) -> std::io::Result<Vec<u8>> {
        let mac = self.mac();
        let mut out = Vec::with_capacity(2 + TICKET_LEN + NONCE_LEN + MAC_LEN);
        // A cached ticket is exactly TICKET_LEN bytes by construction; an
        // impossible overrun fails the request instead of truncating the
        // wire length.
        let ticket_len = u16::try_from(self.ticket.len()).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "resume ticket exceeds the u16 wire length",
            )
        })?;
        out.extend_from_slice(&ticket_len.to_be_bytes());
        out.extend_from_slice(&self.ticket);
        out.extend_from_slice(&self.client_nonce);
        out.extend_from_slice(&mac);
        Ok(out)
    }

    /// Parse a request off the stream (the MAC is carried, not checked,
    /// here — `ServerResumeStore::verify` does that).
    async fn read_from<T: AsyncRead + Unpin>(conn: &mut T) -> Result<Self, std::io::Error> {
        let mut len = [0u8; 2];
        conn.read_exact(&mut len).await?;
        let ticket_len = u16::from_be_bytes(len) as usize;
        if ticket_len != TICKET_LEN {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "resume request carried a malformed ticket length",
            ));
        }
        let mut ticket = vec![0u8; ticket_len];
        conn.read_exact(&mut ticket).await?;
        let mut client_nonce = [0u8; NONCE_LEN];
        conn.read_exact(&mut client_nonce).await?;
        let mut mac = [0u8; MAC_LEN];
        conn.read_exact(&mut mac).await?;
        Ok(ResumeRequest {
            server_static: Vec::new(),
            ticket,
            handshake_hash: [0u8; 32],
            client_nonce,
            mac: Some(mac),
        })
    }
}

/// The responder's half of a resumed connection: read the request,
/// verify it, answer with the verdict and derive the record cipher.
///
/// `Ok(None)` means the request was declined — the caller drops the
/// connection and the initiator falls back to a full handshake.
pub async fn server_resume<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
    store: &ServerResumeStore,
) -> Result<Option<ResumedCipher>, std::io::Error> {
    let request = ResumeRequest::read_from(conn).await?;
    // `read_from` always parses a MAC; a request built locally computes
    // one, so an absent MAC is a programming error, not a wire state.
    let Some(mac) = request.mac else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "a resume request read off the wire always carries a MAC",
        ));
    };
    let Some(hh) = store.verify(&request.ticket, &request.client_nonce, &mac) else {
        conn.write_all(&[VERDICT_DECLINED]).await?;
        conn.flush().await?;
        warn!("declined a noise resume request");
        return Ok(None);
    };
    let server_nonce = random_bytes::<NONCE_LEN>()?;
    let server_mac_key = role_key(&hh, b"server-mac");
    let mut mac_input = Vec::with_capacity(16 + 2 * NONCE_LEN);
    mac_input.extend_from_slice(b"server-verdict");
    mac_input.extend_from_slice(&request.client_nonce);
    mac_input.extend_from_slice(&server_nonce);
    let mac = hmac_sha256(&server_mac_key, &mac_input);
    let mut out = Vec::with_capacity(1 + NONCE_LEN + MAC_LEN);
    out.push(VERDICT_RESUMED);
    out.extend_from_slice(&server_nonce);
    out.extend_from_slice(&mac);
    conn.write_all(&out).await?;
    conn.flush().await?;
    debug!("accepted a resumed noise session");
    Ok(Some(ResumedCipher::responder(
        &hh,
        &request.client_nonce,
        &server_nonce,
    )))
}

/// The initiator's half of a resumed connection: send the request, read
/// the verdict and derive the record cipher.
///
/// `Ok(None)` means the responder declined — the caller drops the cached
/// ticket and falls back to a full handshake on a fresh connection.
pub async fn client_resume<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
    request: &ResumeRequest,
    cache: &ClientResumeCache,
) -> Result<Option<ResumedCipher>, std::io::Error> {
    conn.write_all(&request.encode()?).await?;
    conn.flush().await?;

    let mut status = [0u8; 1];
    conn.read_exact(&mut status).await?;
    if status[0] == VERDICT_DECLINED {
        cache.drop_entry(&request.server_static);
        debug!("server declined the noise resume; falling back");
        return Ok(None);
    }
    if status[0] != VERDICT_RESUMED {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "unknown resume verdict",
        ));
    }
    let mut server_nonce = [0u8; NONCE_LEN];
    conn.read_exact(&mut server_nonce).await?;
    let mut mac = [0u8; MAC_LEN];
    conn.read_exact(&mut mac).await?;
    let server_mac_key = role_key(&request.handshake_hash, b"server-mac");
    let mut mac_input = Vec::with_capacity(16 + 2 * NONCE_LEN);
    mac_input.extend_from_slice(b"server-verdict");
    mac_input.extend_from_slice(&request.client_nonce);
    mac_input.extend_from_slice(&server_nonce);
    if hmac_sha256(&server_mac_key, &mac_input) != mac {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "resume verdict carried a wrong server MAC",
        ));
    }
    Ok(Some(ResumedCipher::initiator(
        &request.handshake_hash,
        &request.client_nonce,
        &server_nonce,
    )))
}

// ------------------------------------------------- ticket exchange records

/// The ticket exchange after a full handshake, from the initiator's side:
/// ask, read the ticket, cache it under the server's static key.
///
/// Runs over the *established* record stream (the exchange rides the
/// handshake's cipher), which is why it is generic over the stream type
/// rather than a raw socket.
pub async fn client_take_ticket<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
    cache: &ClientResumeCache,
    server_static: &[u8],
    handshake_hash: &[u8],
) -> Result<(), std::io::Error> {
    conn.write_all(&[1u8]).await?; // want a ticket
    conn.flush().await?;
    let mut len = [0u8; 2];
    conn.read_exact(&mut len).await?;
    let ticket_len = u16::from_be_bytes(len) as usize;
    if ticket_len == 0 {
        debug!("server did not issue a resume ticket");
        return Ok(());
    }
    if ticket_len != TICKET_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "malformed resume ticket",
        ));
    }
    let mut ticket = vec![0u8; ticket_len];
    conn.read_exact(&mut ticket).await?;
    let mut hh = [0u8; 32];
    let n = handshake_hash.len().min(32);
    hh[..n].copy_from_slice(&handshake_hash[..n]);
    cache.store(server_static, ticket, hh);
    Ok(())
}

/// The ticket exchange after a full handshake, from the responder's side:
/// answer the ask with a fresh ticket (or an empty answer when resume is
/// disabled).
pub async fn server_issue_ticket<T: AsyncRead + AsyncWrite + Unpin>(
    conn: &mut T,
    store: Option<&ServerResumeStore>,
    handshake_hash: &[u8],
) -> Result<(), std::io::Error> {
    let mut want = [0u8; 1];
    conn.read_exact(&mut want).await?;
    let ticket = match (want[0] == 1, store) {
        (true, Some(store)) => store.issue(handshake_hash)?,
        _ => Vec::new(),
    };
    let mut out = Vec::with_capacity(2 + ticket.len());
    // A sealed ticket is exactly TICKET_LEN bytes; an impossible overrun
    // fails the exchange instead of truncating the wire length.
    let ticket_len = u16::try_from(ticket.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "resume ticket exceeds the u16 wire length",
        )
    })?;
    out.extend_from_slice(&ticket_len.to_be_bytes());
    out.extend_from_slice(&ticket);
    conn.write_all(&out).await?;
    conn.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use tokio::io::duplex;

    const SERVER_KEY: [u8; 32] = [3u8; 32];
    const HANDSHAKE_HASH: [u8; 32] = [9u8; 32];

    #[test]
    fn hkdf_matches_rfc5869_test_case_1() {
        // RFC 5869 Appendix A.1 (SHA-256, L = 42).
        let ikm = [0x0b; 22];
        let salt: Vec<u8> = (0u8..=12).collect();
        let info: Vec<u8> = (0xf0u8..=0xf9).collect();
        let mut okm = [0u8; 42];
        hkdf_sha256(&ikm, &salt, &info, &mut okm);
        let expect = hex::decode(
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865",
        )
        .unwrap();
        assert_eq!(&okm[..], &expect[..]);
    }

    fn issued_ticket(store: &ServerResumeStore) -> (Vec<u8>, ClientResumeCache) {
        let ticket = store.issue(&HANDSHAKE_HASH).unwrap();
        let cache = ClientResumeCache::default();
        cache.store(&SERVER_KEY, ticket.clone(), HANDSHAKE_HASH);
        (ticket, cache)
    }

    #[tokio::test]
    async fn a_resumed_pair_speaks_both_directions() {
        let (mut a, mut b) = duplex(64 * 1024);
        let store = std::sync::Arc::new(ServerResumeStore::new(&[7u8; 32]));
        let (_ticket, cache) = issued_ticket(&store);
        let cache = std::sync::Arc::new(cache);

        let request = ResumeRequest::build(&cache, &SERVER_KEY).unwrap().unwrap();
        let initiator = tokio::spawn({
            let cache = std::sync::Arc::clone(&cache);
            async move { client_resume(&mut a, &request, &cache).await }
        });
        let responder = tokio::spawn({
            let store = std::sync::Arc::clone(&store);
            async move { server_resume(&mut b, &store).await }
        });
        let mut init_cipher = initiator.await.unwrap().unwrap().unwrap();
        let mut resp_cipher = responder.await.unwrap().unwrap().unwrap();

        let plain = b"stripes-and-noise";
        let mut enc = vec![0u8; plain.len() + 16];
        let n = init_cipher.encrypt(plain, &mut enc).unwrap();
        let mut dec = vec![0u8; n];
        let m = resp_cipher.decrypt(&enc[..n], &mut dec).unwrap();
        assert_eq!(&dec[..m], &plain[..]);

        // And back the other way, on the responder's counters.
        let mut enc2 = vec![0u8; plain.len() + 16];
        let n2 = resp_cipher.encrypt(plain, &mut enc2).unwrap();
        let mut dec2 = vec![0u8; n2];
        let m2 = init_cipher.decrypt(&enc2[..n2], &mut dec2).unwrap();
        assert_eq!(&dec2[..m2], &plain[..]);
    }

    #[tokio::test]
    async fn a_tampered_ticket_is_declined() {
        let (mut a, mut b) = duplex(64 * 1024);
        let store = ServerResumeStore::new(&[7u8; 32]);
        let (mut ticket, cache) = issued_ticket(&store);
        ticket[20] ^= 0xff; // corrupt the sealed issue time
        cache.store(&SERVER_KEY, ticket, HANDSHAKE_HASH); // cache the tampered copy
        let cache = std::sync::Arc::new(cache);

        let request = ResumeRequest::build(&cache, &SERVER_KEY).unwrap().unwrap();
        let initiator = tokio::spawn({
            let cache = std::sync::Arc::clone(&cache);
            async move { client_resume(&mut a, &request, &cache).await }
        });
        let responder = tokio::spawn(async move { server_resume(&mut b, &store).await });
        assert!(
            responder.await.unwrap().unwrap().is_none(),
            "a tampered ticket resumed"
        );
        assert!(
            initiator.await.unwrap().unwrap().is_none(),
            "the initiator ignored the decline"
        );
        // The declined entry is gone from the client's cache.
        assert!(matches!(
            ResumeRequest::build(&cache, &SERVER_KEY),
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn a_stale_ticket_is_declined() {
        let (mut a, mut b) = duplex(64 * 1024);
        let store = ServerResumeStore::new(&[7u8; 32]);
        let (_ticket, cache) = issued_ticket(&store);
        // Rewind every stored issue time past the TTL.
        for entry in store
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values_mut()
        {
            entry.issued = 0;
        }
        let cache = std::sync::Arc::new(cache);

        let request = ResumeRequest::build(&cache, &SERVER_KEY).unwrap().unwrap();
        let initiator = tokio::spawn({
            let cache = std::sync::Arc::clone(&cache);
            async move { client_resume(&mut a, &request, &cache).await }
        });
        let responder = tokio::spawn(async move { server_resume(&mut b, &store).await });
        assert!(
            responder.await.unwrap().unwrap().is_none(),
            "a stale ticket resumed"
        );
        assert!(initiator.await.unwrap().unwrap().is_none());
    }

    #[tokio::test]
    async fn a_replayed_request_is_declined() {
        let (mut a, mut b) = duplex(64 * 1024);
        let store = std::sync::Arc::new(ServerResumeStore::new(&[7u8; 32]));
        let (_ticket, cache) = issued_ticket(&store);
        let cache = std::sync::Arc::new(cache);

        // First use succeeds.
        let request = ResumeRequest::build(&cache, &SERVER_KEY).unwrap().unwrap();
        let initiator = tokio::spawn({
            let request = request.clone();
            let cache = std::sync::Arc::clone(&cache);
            async move { client_resume(&mut a, &request, &cache).await }
        });
        let responder = tokio::spawn({
            let store = std::sync::Arc::clone(&store);
            async move { server_resume(&mut b, &store).await }
        });
        initiator.await.unwrap().unwrap().unwrap();
        assert!(responder.await.unwrap().unwrap().is_some());

        // The very same request again: the nonce is already reserved.
        let (mut a2, mut b2) = duplex(64 * 1024);
        let initiator = tokio::spawn({
            let request = request.clone();
            let cache = std::sync::Arc::clone(&cache);
            async move { client_resume(&mut a2, &request, &cache).await }
        });
        let responder = tokio::spawn({
            let store = std::sync::Arc::clone(&store);
            async move { server_resume(&mut b2, &store).await }
        });
        assert!(
            responder.await.unwrap().unwrap().is_none(),
            "a replayed resume request was accepted"
        );
        assert!(initiator.await.unwrap().unwrap().is_none());
    }

    #[test]
    fn a_ticket_from_another_server_key_does_not_open() {
        let store_a = ServerResumeStore::new(&[7u8; 32]);
        let store_b = ServerResumeStore::new(&[8u8; 32]);
        let ticket = store_a.issue(&HANDSHAKE_HASH).unwrap();
        // Open with the wrong key: the AEAD tag fails.
        let (mut a, mut b) = (ticket.clone(), ticket.clone());
        assert!(open_ticket_for_test(&store_b.ticket_key, &mut a).is_none());
        assert!(open_ticket_for_test(&store_a.ticket_key, &mut b).is_some());
    }

    fn open_ticket_for_test(key: &[u8; 32], ticket: &mut [u8]) -> Option<(u64, u64, [u8; 32])> {
        open_ticket(key, ticket)
    }

    #[tokio::test]
    async fn the_ticket_exchange_caches_the_server_ticket() {
        // The full-handshake epilogue: the client asks, the server issues,
        // and the stream is afterwards the plain record stream (duplex
        // here stands in for it).
        let (mut a, mut b) = duplex(64 * 1024);
        let store = ServerResumeStore::new(&[7u8; 32]);
        let cache = ClientResumeCache::default();

        let client = tokio::spawn(async move {
            client_take_ticket(&mut a, &cache, &SERVER_KEY, &HANDSHAKE_HASH)
                .await
                .unwrap();
            cache
        });
        tokio::spawn(async move {
            server_issue_ticket(&mut b, Some(&store), &HANDSHAKE_HASH)
                .await
                .unwrap();
        });
        let cache = client.await.unwrap();
        let (ticket, hh) = cache.take(&SERVER_KEY).unwrap();
        assert_eq!(ticket.len(), TICKET_LEN);
        assert_eq!(hh, HANDSHAKE_HASH);
    }

    #[tokio::test]
    async fn a_disabled_server_answers_with_an_empty_ticket() {
        let (mut a, mut b) = duplex(64 * 1024);
        let cache = ClientResumeCache::default();
        let client = tokio::spawn(async move {
            client_take_ticket(&mut a, &cache, &SERVER_KEY, &HANDSHAKE_HASH)
                .await
                .unwrap();
            cache
        });
        tokio::spawn(async move {
            server_issue_ticket(&mut b, None, &HANDSHAKE_HASH)
                .await
                .unwrap();
        });
        let cache = client.await.unwrap();
        assert!(
            cache.take(&SERVER_KEY).is_none(),
            "an empty ticket was cached"
        );
    }
}
