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
//! - writes encrypt straight into the framing buffer behind the two-byte
//!   header and address it by index — no per-record `set_len` dance, and no
//!   `unsafe` anywhere in this module.
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
    task::{Context, Poll, Waker, ready},
};

use pin_project::pin_project;
use snow::{HandshakeState, TransportState};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

const TAG_LEN: usize = 16;
/// Largest Noise message: the wire format carries a `u16` length.
const MAX_MESSAGE_LEN: usize = u16::MAX as usize;
const LENGTH_FIELD_LEN: usize = std::mem::size_of::<u16>();
/// Largest on-wire record: the length header plus a maximal ciphertext.
const MAX_FRAME_LEN: usize = LENGTH_FIELD_LEN + MAX_MESSAGE_LEN;
/// Sentinel for "the record's length header has not been decoded yet".
/// Every real record is at least a tag long, so 0 is never a valid length.
const UNKNOWN_FRAME_LEN: usize = 0;

/// Errors the wrapper surfaces: the handshake or transport state machine
/// failed, or the underlying IO failed.
#[derive(Debug)]
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

type NoiseStreamResult<T> = Result<T, NoiseStreamError>;

#[derive(Debug, Clone, Copy)]
enum ReadState {
    /// At a record boundary: `read_scratch[read_start..read_filled]` holds
    /// zero or more bytes of the next record — the head of a record that
    /// coalesced with its predecessor in a single read wake.
    ReadingRecord,
    /// A decrypted record is being served from `read_payload_buffer`;
    /// `served` of its bytes have already reached the caller.
    ServingPayload { served: usize },
    /// EOF or shutdown: further reads return Ok with nothing filled.
    ShuttingDown,
}

#[derive(Debug, Clone, Copy)]
enum WriteState {
    Idle,
    /// The record `write_message_buffer[start..end]` is being written to
    /// the inner stream; it carries `payload_len` plaintext bytes.
    WritingMessage {
        start: usize,
        end: usize,
        payload_len: usize,
    },
    ShuttingDown,
}

#[pin_project]
pub struct NoiseStream<T> {
    #[pin]
    inner: T,

    transport: TransportState,
    read_state: ReadState,
    write_state: WriteState,
    write_clean_waker: Option<Waker>,

    /// Ciphertext accumulation buffer, allocated once at `MAX_FRAME_LEN`:
    /// `read_scratch[read_start..read_filled]` are the bytes of the record
    /// in progress, and a successor record may already sit behind it,
    /// consumed in place without copying. `read_expected` is the record's
    /// total wire length once its header has been decoded, else
    /// `UNKNOWN_FRAME_LEN`.
    read_scratch: Vec<u8>,
    read_start: usize,
    read_filled: usize,
    read_expected: usize,

    /// Decrypt output of the current record: `payload_len` valid bytes.
    read_payload_buffer: Vec<u8>,
    payload_len: usize,

    /// Write framing buffer, allocated once at `LENGTH_FIELD_LEN +
    /// MAX_MESSAGE_LEN`: the length header at `[..LENGTH_FIELD_LEN]`, the
    /// ciphertext behind it.
    write_message_buffer: Vec<u8>,
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
    pub async fn handshake_with_verifier<F: FnOnce(&[u8]) -> NoiseStreamResult<()>>(
        mut inner: T,
        mut state: HandshakeState,
        verifier: F,
    ) -> Result<Self, NoiseStreamError> {
        let mut f = Some(verifier);
        loop {
            if state.is_handshake_finished() {
                let transport = state.into_transport_mode()?;
                return Ok(Self {
                    inner,
                    transport,
                    read_state: ReadState::ReadingRecord,
                    write_state: WriteState::Idle,
                    write_clean_waker: None,
                    read_scratch: vec![0; MAX_FRAME_LEN],
                    read_start: 0,
                    read_filled: 0,
                    read_expected: UNKNOWN_FRAME_LEN,
                    read_payload_buffer: vec![0; MAX_MESSAGE_LEN],
                    payload_len: 0,
                    write_message_buffer: vec![0; LENGTH_FIELD_LEN + MAX_MESSAGE_LEN],
                });
            }

            let mut message = vec![0; MAX_MESSAGE_LEN];
            let mut payload = vec![0; MAX_MESSAGE_LEN];

            if state.is_my_turn() {
                let len = state.write_message(&[], &mut message)?;
                // Lengths are bounded by MAX_MESSAGE_LEN (u16::MAX):
                // snow rejects larger messages, so the cast cannot
                // truncate.
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "message length is bounded by the u16 wire format"
                )]
                inner.write_u16_le(len as u16).await?;
                inner.write_all(&message[..len]).await?;
                inner.flush().await?;
            } else {
                let len = inner.read_u16_le().await? as usize;
                inner.read_exact(&mut message[..len]).await?;
                state.read_message(&message[..len], &mut payload)?;
                if let Some(pubkey) = state.get_remote_static()
                    && let Some(verifier) = f.take()
                {
                    verifier(pubkey)?;
                }
            }
        }
    }

    #[inline]
    pub async fn handshake(inner: T, state: HandshakeState) -> Result<Self, NoiseStreamError> {
        Self::handshake_with_verifier(inner, state, |_| Ok(())).await
    }
}

impl<T> AsyncWrite for NoiseStream<T>
where
    T: AsyncWrite,
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
        let write_message_buffer = this.write_message_buffer;

        loop {
            match *state {
                WriteState::ShuttingDown => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                WriteState::Idle => {
                    let payload_len = buf.len().min(MAX_MESSAGE_LEN - TAG_LEN);
                    let message_len = transport
                        .write_message(
                            &buf[..payload_len],
                            &mut write_message_buffer[LENGTH_FIELD_LEN..],
                        )
                        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                    // `message_len` is bounded by MAX_MESSAGE_LEN
                    // (u16::MAX), so the cast cannot truncate.
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "message length is bounded by the u16 wire format"
                    )]
                    write_message_buffer[..LENGTH_FIELD_LEN]
                        .copy_from_slice(&(message_len as u16).to_le_bytes());
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
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        let this = self.project();
        match this.write_state {
            WriteState::ShuttingDown | WriteState::Idle => {
                return Poll::Ready(Ok(()));
            }
            WriteState::WritingMessage { .. } => {}
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
                    read_buf.put_slice(&this.read_payload_buffer[served..served + take]);
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
                            this.read_scratch[*this.read_start],
                            this.read_scratch[*this.read_start + 1],
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
                        let ciphertext = &this.read_scratch
                            [start + LENGTH_FIELD_LEN..start + *this.read_expected];
                        let n = transport
                            .read_message(ciphertext, &mut this.read_payload_buffer[..])
                            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                        *this.read_start += *this.read_expected;
                        *this.read_expected = UNKNOWN_FRAME_LEN;
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
                        this.read_scratch
                            .copy_within(*this.read_start..*this.read_filled, 0);
                        *this.read_filled -= *this.read_start;
                        *this.read_start = 0;
                    }

                    let mut scratch_read_buf =
                        ReadBuf::new(&mut this.read_scratch[*this.read_filled..MAX_FRAME_LEN]);
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
    async fn zero_length_write_is_not_eof() {
        let (mut c, mut s) = pair().await;
        c.write_all(&[1, 2, 3]).await.unwrap();
        c.write_all(&[]).await.unwrap();
        c.write_all(&[4, 5, 6]).await.unwrap();
        c.flush().await.unwrap();
        let mut buf = [0u8; 6];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, &[1, 2, 3, 4, 5, 6]);
    }
}
