//! u16-framed Noise record stream over any tokio IO type.
//!
//! One Noise transport-mode message per record: a `u16` payload length +
//! ciphertext (+16-byte tag). Reads are record-oriented; writes encrypt one
//! record per call. This is the record stream that used to be the
//! `snowstorm` crate's `NoiseStream`, maintained in-repo since the snow 0.10
//! upgrade and deliberately leaner than the upstream copy:
//!
//! - reads accumulate the length header **and** the ciphertext in one
//!   buffer — one `poll_read` sweep per record instead of a separate
//!   two-byte header read first, and a record that coalesces with its
//!   successor in a single wake is decrypted from the same buffer with no
//!   extra copy;
//! - when the caller's buffer holds a whole record (the plaintext length is
//!   known from the ciphertext length, so the output region can be sized
//!   exactly), the decrypt writes **straight into the caller's buffer**
//!   through tokio's `ReadBuf::initialize_unfilled_to` — no staging copy.
//!   A caller that asks for less than a record (the yamux frame reader's
//!   12-byte header and body reads, a small `read_exact`) still gets the
//!   plaintext staged and served progressively;
//! - writes encrypt straight into the framing buffer behind the two-byte
//!   header and address it by index — no per-record `set_len` dance, and no
//!   `unsafe` anywhere in this module. A transport that takes owned records
//!   (`AsyncWriteOwned`, mhd's writer channel) gets the record as one
//!   `Bytes` instead, which removes the copy at its write boundary;
//! - the setup path allocates almost nothing: the handshake runs on stack
//!   buffers (its messages are bounded by the pattern's tokens, well under
//!   300 bytes), and the three 64 KiB record buffers come from a bounded
//!   pool — freed in one piece they exceed the allocator's trim threshold
//!   and the next connection would re-fault and re-zero every page, which
//!   measured at ~1/6 of the connection-setup CPU.
//!
//! Ported from snowstorm 0.4.0 (<https://github.com/black-binary/snowstorm>),
//! Apache-2.0. Changes vs upstream: the error type is trimmed to what this
//! wrapper uses, `snow` is bumped to 0.10, `futures_util::ready` is
//! replaced by `std::task::ready`, and the upstream re-exports/tests are
//! dropped (the e2e integration suite covers the wrapper).

use std::{
    fmt::Debug,
    io::ErrorKind,
    pin::Pin,
    sync::Mutex,
    task::{Context, Poll, Waker, ready},
};

use pin_project::pin_project;
use snow::{HandshakeState, TransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use bytes::{Bytes, BytesMut};

use crate::common::owned_write::AsyncWriteOwned;

const TAG_LEN: usize = 16;
/// Largest Noise message: the wire format carries a `u16` length.
const MAX_MESSAGE_LEN: usize = u16::MAX as usize;
/// Bound for a *handshake* message (not a record): every pattern the
/// config accepts stays far below this — see `handshake_with_verifier`.
const MAX_HANDSHAKE_MESSAGE: usize = 1024;
const LENGTH_FIELD_LEN: usize = std::mem::size_of::<u16>();
/// Largest on-wire record: the length header plus a maximal ciphertext.
const MAX_FRAME_LEN: usize = LENGTH_FIELD_LEN + MAX_MESSAGE_LEN;
/// Sentinel for "the record's length header has not been decoded yet".
/// Every real record is at least a tag long, so 0 is never a valid length.
const UNKNOWN_FRAME_LEN: usize = 0;

/// Upper bound on pooled buffer sets. Each set is 192 KiB, so the pool
/// retains at most ~12 MiB — and only up to the connection high-water mark,
/// never preallocated.
const RECORD_POOL_CAP: usize = 64;

/// The three per-connection record buffers of a [`NoiseStream`], pooled.
///
/// Measured on a connection-setup probe (release build, one host): the
/// 192 KiB a stream allocates is not cheap to churn. Freed in one piece it
/// exceeds glibc's 128 KiB trim threshold, so the allocator returns it to
/// the OS and the next connection re-faults and re-zeroes every page — 48
/// minor faults per connection, ~1/6 of the pair-setup CPU. A bounded
/// free-list keeps a warm set around instead: no faults, no memset, no
/// malloc traffic per connection.
///
/// Reused buffers keep their old contents (ciphertext and plaintext from
/// the previous connection). That is safe — every region is written before
/// it is read: the scratch is filled by `poll_read` before parsing, the
/// payload buffer by the decrypt before serving, and the write buffer by
/// the encrypt before the length header is set — and nothing stale reaches
/// the wire. It does mean plaintext lingers in pooled memory until reuse,
/// exactly as it does in any freed buffer.
struct RecordBuffers {
    /// Ciphertext accumulation for reads (header + ciphertext of one record).
    scratch: Vec<u8>,
    /// Decrypt output for records served progressively.
    payload: Vec<u8>,
    /// Framing buffer for writes (length header + ciphertext).
    write: Vec<u8>,
}

static RECORD_POOL: Mutex<Vec<RecordBuffers>> = Mutex::new(Vec::new());

impl RecordBuffers {
    fn new() -> Self {
        RecordBuffers {
            scratch: vec![0; MAX_FRAME_LEN],
            payload: vec![0; MAX_MESSAGE_LEN],
            write: vec![0; LENGTH_FIELD_LEN + MAX_MESSAGE_LEN],
        }
    }

    /// Take a set from the pool, or allocate a fresh one.
    fn take() -> Self {
        match RECORD_POOL.lock() {
            Ok(mut pool) => pool.pop().unwrap_or_else(Self::new),
            // A poisoned pool lock only means some thread panicked while
            // holding it; the buffer sets themselves stay valid.
            Err(poisoned) => poisoned.into_inner().pop().unwrap_or_else(Self::new),
        }
    }
}

impl Drop for RecordBuffers {
    fn drop(&mut self) {
        let mut pool = match RECORD_POOL.lock() {
            Ok(pool) => pool,
            Err(poisoned) => poisoned.into_inner(),
        };
        if pool.len() < RECORD_POOL_CAP {
            pool.push(RecordBuffers {
                scratch: std::mem::take(&mut self.scratch),
                payload: std::mem::take(&mut self.payload),
                write: std::mem::take(&mut self.write),
            });
        }
        // Over the cap the set is simply dropped, which frees it.
    }
}

/// Errors the wrapper surfaces: the handshake or transport state machine
/// failed, or the underlying IO failed.
#[derive(Debug)]
#[doc(hidden)]
pub enum NoiseStreamError {
    Snow(snow::Error),
    Io(std::io::Error),
}

impl From<snow::Error> for NoiseStreamError {
    fn from(e: snow::Error) -> Self {
        Self::Snow(e)
    }
}

impl From<std::io::Error> for NoiseStreamError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl std::fmt::Display for NoiseStreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Snow(e) => write!(f, "noise error: {e}"),
            Self::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for NoiseStreamError {}

#[derive(Debug, Clone, Copy)]
enum ReadState {
    /// At a record boundary: `scratch[read_start..read_filled]` holds
    /// zero or more bytes of the next record — the head of a record that
    /// coalesced with its predecessor in a single read wake.
    ReadingRecord,
    /// A decrypted record is being served from the payload buffer;
    /// `served` of its bytes have already reached the caller.
    ServingPayload { served: usize },
    /// EOF or shutdown: further reads return Ok with nothing filled.
    ShuttingDown,
}

#[derive(Debug, Clone)]
enum WriteState {
    Idle,
    /// The record `write[start..end]` is being written to
    /// the inner stream; it carries `payload_len` plaintext bytes.
    WritingMessage {
        start: usize,
        end: usize,
        payload_len: usize,
    },
    /// An owned record in flight on a `TAKES_OWNED` transport: the
    /// transport has accepted `offset` bytes of `record` (header +
    /// ciphertext), which carries `payload_len` plaintext bytes.
    WritingOwned {
        record: Bytes,
        offset: usize,
        payload_len: usize,
    },
    ShuttingDown,
}

#[pin_project]
#[doc(hidden)]
pub struct NoiseStream<T> {
    #[pin]
    inner: T,

    transport: TransportState,
    read_state: ReadState,
    write_state: WriteState,
    write_clean_waker: Option<Waker>,

    /// The pooled record buffers: `scratch[read_start..read_filled]` are
    /// the bytes of the record in progress, and a successor record may
    /// already sit behind it, consumed in place without copying.
    /// `read_expected` is the record's total wire length once its header
    /// has been decoded, else `UNKNOWN_FRAME_LEN`.
    bufs: RecordBuffers,
    read_start: usize,
    read_filled: usize,
    read_expected: usize,

    /// Decrypt output of the current record: `payload_len` valid bytes.
    payload_len: usize,
}

impl<T: Debug> Debug for NoiseStream<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoiseStream")
            .field("inner", &self.inner)
            .field("read_state", &self.read_state)
            .field("write_state", &self.write_state)
            .field("write_clean_waker", &self.write_clean_waker)
            .finish_non_exhaustive()
    }
}

impl<T> NoiseStream<T> {
    /// The underlying transport stream (socket options are applied through
    /// this by the transport layer's `hint`).
    pub fn get_inner(&self) -> &T {
        &self.inner
    }
}

impl<T> NoiseStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Run the fixed KK handshake over the inner stream.
    ///
    /// Handshake messages are bounded by KK's tokens (two key exchanges plus
    /// tags), far below the buffer below; snow rejects a larger message, so a
    /// corrupt frame fails loudly instead of truncating.
    ///
    /// # Errors
    ///
    /// Fails when the peer's frame is malformed or the handshake does not
    /// verify.
    pub async fn handshake(
        mut inner: T,
        mut state: HandshakeState,
    ) -> Result<Self, NoiseStreamError> {
        let mut message = [0u8; MAX_HANDSHAKE_MESSAGE];
        let mut payload = [0u8; MAX_HANDSHAKE_MESSAGE];
        // Taken before the loop so a failed handshake returns the set to the
        // pool through its Drop.
        let bufs = RecordBuffers::take();
        loop {
            if state.is_handshake_finished() {
                let transport = state.into_transport_mode()?;
                return Ok(Self {
                    inner,
                    transport,
                    read_state: ReadState::ReadingRecord,
                    write_state: WriteState::Idle,
                    write_clean_waker: None,
                    bufs,
                    read_start: 0,
                    read_filled: 0,
                    read_expected: UNKNOWN_FRAME_LEN,
                    payload_len: 0,
                });
            }

            if state.is_my_turn() {
                let len = state.write_message(&[], &mut message)?;
                // The u16 length field is the wire format's own bound and
                // snow rejects a larger message, so the conversion cannot
                // fail; an impossible failure surfaces as a stream error
                // rather than a panic.
                let len = u16::try_from(len).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "noise message length exceeds the u16 wire format",
                    )
                })?;
                inner.write_u16_le(len).await?;
                inner.write_all(&message[..usize::from(len)]).await?;
                inner.flush().await?;
            } else {
                let len = inner.read_u16_le().await? as usize;
                if len > MAX_HANDSHAKE_MESSAGE {
                    return Err(NoiseStreamError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "handshake message too long",
                    )));
                }
                inner.read_exact(&mut message[..len]).await?;
                state.read_message(&message[..len], &mut payload)?;
            }
        }
    }
}

impl<T> AsyncWrite for NoiseStream<T>
where
    T: AsyncWriteOwned,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.project();
        let mut inner = this.inner;
        let state = this.write_state;
        let transport = this.transport;
        let write_message_buffer = &mut this.bufs.write;

        loop {
            match state {
                WriteState::ShuttingDown => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                WriteState::Idle => {
                    let payload_len = buf.len().min(MAX_MESSAGE_LEN - TAG_LEN);
                    if payload_len > 0 && T::TAKES_OWNED {
                        // The owned-record path: encrypt into a fresh
                        // buffer and hand the whole record to the
                        // transport by ownership. The channel boundary
                        // copy disappears — the record buffer *is* the
                        // transport's buffer — and the only copy left on
                        // this path is the AEAD's own. `zeroed` pays one
                        // memset the encrypt immediately overwrites; the
                        // alternative (uninitialized memory handed to the
                        // cipher) is not worth an unsafe block here.
                        let mut record = BytesMut::zeroed(LENGTH_FIELD_LEN + payload_len + TAG_LEN);
                        let message_len = transport
                            .write_message(&buf[..payload_len], &mut record[LENGTH_FIELD_LEN..])
                            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                        // `message_len` is bounded by MAX_MESSAGE_LEN
                        // (u16::MAX); an impossible overrun is an error,
                        // not a truncated record.
                        let len = u16::try_from(message_len).map_err(|_| {
                            std::io::Error::new(
                                ErrorKind::InvalidData,
                                "noise record length exceeds the u16 wire format",
                            )
                        })?;
                        record[..LENGTH_FIELD_LEN].copy_from_slice(&len.to_le_bytes());
                        record.truncate(LENGTH_FIELD_LEN + message_len);
                        *state = WriteState::WritingOwned {
                            record: record.freeze(),
                            offset: 0,
                            payload_len,
                        };
                        continue;
                    }
                    let message_len = transport
                        .write_message(
                            &buf[..payload_len],
                            &mut write_message_buffer[LENGTH_FIELD_LEN..],
                        )
                        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                    // `message_len` is bounded by MAX_MESSAGE_LEN
                    // (u16::MAX); an impossible overrun is an error, not
                    // a truncated record.
                    let len = u16::try_from(message_len).map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::InvalidData,
                            "noise record length exceeds the u16 wire format",
                        )
                    })?;
                    write_message_buffer[..LENGTH_FIELD_LEN].copy_from_slice(&len.to_le_bytes());
                    *state = WriteState::WritingMessage {
                        start: 0,
                        end: LENGTH_FIELD_LEN + message_len,
                        payload_len,
                    };
                }
                WriteState::WritingMessage {
                    start,
                    end,
                    payload_len,
                } => {
                    // Copy the small fields out so the state can be
                    // re-assigned below (the match holds a borrow of it).
                    let (start, end, payload_len) = (*start, *end, *payload_len);
                    let n = ready!(
                        Pin::new(&mut inner).poll_write(cx, &write_message_buffer[start..end])
                    )?;
                    let start = start + n;

                    if start == end {
                        *state = WriteState::Idle;
                        if let Some(waker) = this.write_clean_waker.take() {
                            waker.wake();
                        }
                        return Poll::Ready(Ok(payload_len));
                    }
                    *state = WriteState::WritingMessage {
                        start,
                        end,
                        payload_len,
                    };
                }
                WriteState::WritingOwned {
                    record,
                    offset,
                    payload_len,
                } => {
                    // The `Bytes` handle clone is O(1); the copy ends the
                    // borrow of the state so it can be re-assigned below.
                    let (record, offset, payload_len) = (record.clone(), *offset, *payload_len);
                    let n = ready!(inner.as_mut().poll_write_owned(cx, record.slice(offset..)))?;
                    let offset = offset + n;

                    if offset == record.len() {
                        *state = WriteState::Idle;
                        if let Some(waker) = this.write_clean_waker.take() {
                            waker.wake();
                        }
                        return Poll::Ready(Ok(payload_len));
                    }
                    *state = WriteState::WritingOwned {
                        record,
                        offset,
                        payload_len,
                    };
                }
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.project();
        match this.write_state {
            WriteState::ShuttingDown | WriteState::Idle => {
                return Poll::Ready(Ok(()));
            }
            WriteState::WritingMessage { .. } | WriteState::WritingOwned { .. } => {}
        }

        *this.write_clean_waker = Some(cx.waker().clone());
        ready!(this.inner.poll_flush(cx))?;
        Poll::Pending
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.project();
        if let Some(waker) = this.write_clean_waker.take() {
            waker.wake();
        }
        *this.write_state = WriteState::ShuttingDown;
        this.inner.poll_shutdown(cx)
    }
}

impl<T> AsyncRead for NoiseStream<T>
where
    T: AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        read_buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.project();

        let mut inner = this.inner;
        let state = this.read_state;
        let transport = this.transport;

        loop {
            match *state {
                ReadState::ShuttingDown => {
                    return Poll::Ready(Ok(()));
                }
                ReadState::ServingPayload { served } => {
                    let take = (*this.payload_len - served).min(read_buf.remaining());
                    read_buf.put_slice(&this.bufs.payload[served..served + take]);
                    let served = served + take;

                    if served == *this.payload_len {
                        *state = ReadState::ReadingRecord;
                    } else {
                        *state = ReadState::ServingPayload { served };
                    }
                    return Poll::Ready(Ok(()));
                }
                ReadState::ReadingRecord => {
                    // Every buffered record consumed: slide the window back
                    // to the front so the scratch buffer never drifts.
                    if *this.read_start == *this.read_filled {
                        *this.read_start = 0;
                        *this.read_filled = 0;
                    }

                    // Decode the length header as soon as two bytes of the
                    // record are in the buffer. The field is the ciphertext
                    // length with the tag included — the same value the
                    // writer stores there — so the record is the header
                    // plus exactly that many bytes.
                    if *this.read_expected == UNKNOWN_FRAME_LEN
                        && *this.read_filled - *this.read_start >= LENGTH_FIELD_LEN
                    {
                        let header = [
                            this.bufs.scratch[*this.read_start],
                            this.bufs.scratch[*this.read_start + 1],
                        ];
                        *this.read_expected =
                            LENGTH_FIELD_LEN + usize::from(u16::from_le_bytes(header));
                    }

                    // The whole record is buffered: decrypt it straight from
                    // the accumulation buffer.
                    if *this.read_expected != UNKNOWN_FRAME_LEN
                        && *this.read_filled - *this.read_start >= *this.read_expected
                    {
                        let start = *this.read_start;
                        let ciphertext = &this.bufs.scratch
                            [start + LENGTH_FIELD_LEN..start + *this.read_expected];
                        let plaintext_len = *this.read_expected - LENGTH_FIELD_LEN - TAG_LEN;
                        *this.read_start += *this.read_expected;
                        *this.read_expected = UNKNOWN_FRAME_LEN;

                        if plaintext_len > 0 && read_buf.remaining() >= plaintext_len {
                            // The caller's buffer holds the whole record:
                            // decrypt straight into it, no staging copy. The
                            // plaintext length is known from the ciphertext
                            // length (a fixed-size tag), so the output region
                            // can be sized exactly — and
                            // `initialize_unfilled_to` is a plain slice view
                            // for the standard `ReadBuf::new` caller (the
                            // region is already initialized) that only pays
                            // a memset for `ReadBuf::uninit` callers.
                            let out = read_buf.initialize_unfilled_to(plaintext_len);
                            let n = transport
                                .read_message(ciphertext, out)
                                .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                            if n > 0 {
                                read_buf.advance(n);
                                return Poll::Ready(Ok(()));
                            }
                            // Zero plaintext from a non-empty record cannot
                            // happen with an AEAD; consume and continue.
                            continue;
                        }

                        // The caller's buffer is smaller than the record (or
                        // the record is empty): stage the plaintext and
                        // serve it progressively.
                        let n = transport
                            .read_message(ciphertext, &mut this.bufs.payload[..])
                            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                        *this.payload_len = n;
                        if n > 0 {
                            *state = ReadState::ServingPayload { served: 0 };
                            continue;
                        }
                        // A zero-length record carries no bytes for the
                        // caller: fall through to the next record instead
                        // of surfacing an EOF-shaped empty read.
                        continue;
                    }

                    // The record is incomplete: read more. A completely
                    // full buffer is compacted to the front first — the
                    // record in progress always fits once it starts at
                    // offset 0.
                    if *this.read_filled == MAX_FRAME_LEN {
                        this.bufs
                            .scratch
                            .copy_within(*this.read_start..*this.read_filled, 0);
                        *this.read_filled -= *this.read_start;
                        *this.read_start = 0;
                    }

                    let mut scratch_read_buf =
                        ReadBuf::new(&mut this.bufs.scratch[*this.read_filled..MAX_FRAME_LEN]);
                    ready!(inner.as_mut().poll_read(cx, &mut scratch_read_buf))?;
                    let n = scratch_read_buf.filled().len();
                    if n == 0 {
                        // EOF: a partial record at the tail is dropped and
                        // the caller sees the stream end.
                        *state = ReadState::ShuttingDown;
                    } else {
                        *this.read_filled += n;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        reason = "tests unwrap values they just constructed"
    )]
    use super::*;
    use snow::Builder;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    impl AsyncWriteOwned for tokio::io::DuplexStream {}

    /// A duplex wrapper that opts into the owned-record path, so the
    /// existing round-trip tests exercise `TAKES_OWNED = true` end to end.
    struct OwnedDuplex(tokio::io::DuplexStream);

    impl AsyncWriteOwned for OwnedDuplex {
        const TAKES_OWNED: bool = true;
    }

    impl AsyncRead for OwnedDuplex {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for OwnedDuplex {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            std::pin::Pin::new(&mut self.0).poll_write(cx, buf)
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_flush(cx)
        }
        fn poll_shutdown(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    async fn pair() -> (
        NoiseStream<tokio::io::DuplexStream>,
        NoiseStream<tokio::io::DuplexStream>,
    ) {
        let (a, b) = duplex(1024 * 1024);
        let params: snow::params::NoiseParams =
            "Noise_NN_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
        let init = Builder::new(params.clone()).build_initiator().unwrap();
        let resp = Builder::new(params).build_responder().unwrap();
        let (c, s) = tokio::join!(
            NoiseStream::handshake(a, init),
            NoiseStream::handshake(b, resp)
        );
        (c.unwrap(), s.unwrap())
    }

    /// A `pair` over the owned-record path: the client's writes encrypt
    /// into a fresh buffer and cross as one owned `Bytes` per record.
    async fn owned_pair() -> (NoiseStream<OwnedDuplex>, NoiseStream<OwnedDuplex>) {
        let (a, b) = duplex(1024 * 1024);
        let params: snow::params::NoiseParams =
            "Noise_NN_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
        let init = Builder::new(params.clone()).build_initiator().unwrap();
        let resp = Builder::new(params).build_responder().unwrap();
        let (c, s) = tokio::join!(
            NoiseStream::handshake(OwnedDuplex(a), init),
            NoiseStream::handshake(OwnedDuplex(b), resp)
        );
        (c.unwrap(), s.unwrap())
    }

    #[tokio::test]
    async fn roundtrip_sizes() {
        let (mut c, mut s) = pair().await;
        for size in [1usize, 100, 1000, 16 * 1024, 32 * 1024, 65519, 65535, 70000] {
            let data: Vec<u8> = (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect();
            c.write_all(&data).await.unwrap();
            c.flush().await.unwrap();
            let mut buf = vec![0; size];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, data, "size {size}");
        }
    }

    #[tokio::test]
    async fn roundtrip_sizes_through_the_owned_record_path() {
        // Same byte-stream contract as `roundtrip_sizes`, but every write
        // takes the owned-record path (encrypt into a fresh buffer, hand
        // the record to the transport by ownership). The record state
        // machine's partial-write bookkeeping is exercised by the sizes
        // that exceed one `poll_write` (the duplex buffer is 1 MiB, so
        // this drives the single-write completion path; the 64 KiB+ sizes
        // cross the Noise record split and the staging path together).
        let (mut c, mut s) = owned_pair().await;
        for size in [1usize, 100, 1000, 16 * 1024, 32 * 1024, 65519, 70000] {
            let data: Vec<u8> = (0..size).map(|i| u8::try_from(i % 251).unwrap()).collect();
            c.write_all(&data).await.unwrap();
            c.flush().await.unwrap();
            let mut buf = vec![0; size];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, data, "size {size}");
        }
    }

    #[tokio::test]
    async fn many_small_records_then_read() {
        let (mut c, mut s) = pair().await;
        let mut expected = Vec::with_capacity(400);
        for i in 0..100u32 {
            c.write_all(&i.to_le_bytes()).await.unwrap();
            expected.extend_from_slice(&i.to_le_bytes());
        }
        c.flush().await.unwrap();
        let mut buf = vec![0; expected.len()];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, expected);
    }

    #[tokio::test]
    async fn small_caller_buffer_stages_the_record() {
        let (mut c, mut s) = pair().await;
        let data: Vec<u8> = (0..1000u32)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        c.write_all(&data).await.unwrap();
        c.flush().await.unwrap();
        // 10-byte reads are far below the record size, so every read goes
        // through the staged path; it must never surface as an EOF.
        let mut got = Vec::with_capacity(data.len());
        let mut buf = [0u8; 10];
        while got.len() < data.len() {
            let n = s.read(&mut buf).await.unwrap();
            assert!(n > 0, "staged path must not surface EOF mid-record");
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn exact_fit_and_coalesced_records() {
        let (mut c, mut s) = pair().await;
        let small = vec![7u8; 100];
        let large = vec![9u8; 40_000];
        c.write_all(&small).await.unwrap();
        c.write_all(&large).await.unwrap();
        c.flush().await.unwrap();
        // One buffer spanning both records: the first read serves the small
        // record, the second the large one at the exact-fit boundary.
        let mut buf = vec![0; small.len() + large.len()];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf[..small.len()], &small[..]);
        assert_eq!(&buf[small.len()..], &large[..]);
    }

    #[tokio::test]
    async fn empty_record_does_not_look_like_eof() {
        let (mut c, mut s) = pair().await;
        // `write_all` never calls poll_write with an empty buffer, so drive
        // the empty record through poll_write directly.
        let n = std::future::poll_fn(|cx| std::pin::Pin::new(&mut c).poll_write(cx, &[]))
            .await
            .unwrap();
        assert_eq!(n, 0);
        c.write_all(&[1, 2, 3]).await.unwrap();
        c.flush().await.unwrap();
        let mut buf = [0u8; 3];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, &[1, 2, 3]);
    }

    /// Phase-level attribution of the connection-setup cost.
    ///
    /// **Run in release mode** (`cargo test --release`): curve25519-dalek
    /// is 50-100x slower in debug, so the absolute numbers here say
    /// nothing about production. What the phases show is the *split*:
    /// which turns carry the DH exchanges (in this pattern: `es` on the
    /// initiator's first turn, `ee` on the responder's, `ss` on the
    /// initiator's second) and what the symmetric-only remainder is.
    ///
    /// That split is the input for a session-resume design: resume
    /// replaces the DH turns with a cached-secret proof, so the
    /// removable share is the sum of the DH turns.
    #[test]
    fn handshake_phase_attribution() {
        use snow::params::NoiseParams;
        use std::time::Instant;

        // The production default pattern: the client knows the server's
        // static key (NK), the server holds it (configured, generated once
        // here exactly like a config-loaded key).
        const N: usize = 500;
        let params: NoiseParams = "Noise_NK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
        let server_keypair = Builder::new(params.clone()).generate_keypair().unwrap();
        let server_private = server_keypair.private;
        let server_public = server_keypair.public;
        let mut msg = [0u8; MAX_HANDSHAKE_MESSAGE];
        let mut payload = [0u8; MAX_HANDSHAKE_MESSAGE];

        // (a) Initiator turn 1: build + write message 1 (`e`, `es` -> 1 DH).
        let mut build_us = 0f64;
        let mut init_turn1_us = 0f64;
        for _ in 0..N {
            let t = Instant::now();
            let mut init = Builder::new(params.clone())
                .remote_public_key(&server_public)
                .unwrap()
                .build_initiator()
                .unwrap();
            build_us += t.elapsed().as_secs_f64() * 1e6;
            let t = Instant::now();
            let len = init.write_message(&[], &mut msg).unwrap();
            init_turn1_us += t.elapsed().as_secs_f64() * 1e6;
            assert!(len > 0);
        }
        // (b) Responder turn: read message 1, write message 2 (`e`, `ee`).
        let mut resp_turn_us = 0f64;
        // (c) Initiator turn 2: read message 2, write message 3 (`s`, `ss`).
        let mut init_turn2_us = 0f64;
        // (d) Responder finish: read message 3, into transport mode.
        let mut resp_finish_us = 0f64;
        for _ in 0..N {
            let mut init = Builder::new(params.clone())
                .remote_public_key(&server_public)
                .unwrap()
                .build_initiator()
                .unwrap();
            let len = init.write_message(&[], &mut msg).unwrap();
            let msg1 = msg[..len].to_vec();

            let t = Instant::now();
            let mut resp = Builder::new(params.clone())
                .local_private_key(&server_private)
                .unwrap()
                .build_responder()
                .unwrap();
            resp.read_message(&msg1, &mut payload).unwrap();
            let len = resp.write_message(&[], &mut msg).unwrap();
            resp_turn_us += t.elapsed().as_secs_f64() * 1e6;
            let msg2 = msg[..len].to_vec();

            // NK is a two-message pattern: the initiator's last act is
            // reading message 2, the responder's is writing it.
            let t = Instant::now();
            init.read_message(&msg2, &mut payload).unwrap();
            assert!(init.is_handshake_finished());
            init_turn2_us += t.elapsed().as_secs_f64() * 1e6;

            let t = Instant::now();
            assert!(resp.is_handshake_finished());
            let _transport = resp.into_transport_mode().unwrap();
            resp_finish_us += t.elapsed().as_secs_f64() * 1e6;
        }
        // `per` divides by N; converting the loop count through u32 keeps
        // the division exact (N is far below 2^32).
        let per = |us_total: f64| us_total / f64::from(u32::try_from(N).unwrap_or(u32::MAX));
        eprintln!(
            "handshake phase attribution (release build, N={N}):\n  \
             build_initiator:        {:7.2} us\n  \
             initiator turn1 (e,es): {:7.2} us\n  \
             responder turn (e,ee):  {:7.2} us\n  \
             initiator turn2 (read ee): {:7.2} us\n  \
             responder finish:       {:7.2} us\n  \
             state-machine sum:      {:7.2} us",
            per(build_us),
            per(init_turn1_us),
            per(resp_turn_us),
            per(init_turn2_us),
            per(resp_finish_us),
            per(build_us + init_turn1_us + resp_turn_us + init_turn2_us + resp_finish_us),
        );
    }

    #[tokio::test]
    async fn pooled_buffers_do_not_leak_stale_bytes() {
        // Cycle the pool: every pair takes a buffer set that a previous
        // pair may have used, with different contents each round.
        for round in 0..8u8 {
            let (mut c, mut s) = pair().await;
            let payload: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8 ^ round).collect();
            c.write_all(&payload).await.unwrap();
            c.flush().await.unwrap();
            let mut buf = vec![0; payload.len()];
            s.read_exact(&mut buf).await.unwrap();
            assert_eq!(buf, payload, "round {round}");
        }
    }
}
