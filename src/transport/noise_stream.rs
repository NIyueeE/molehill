//! u16-framed Noise record stream over any tokio IO type.
//!
//! One Noise transport-mode message per record: `u16` payload length +
//! ciphertext (+16-byte tag). Reads are record-oriented (the length header
//! is read first, then the full message, then the decrypted payload is
//! served); writes encrypt the whole buffer into one record and push it
//! with a single write. This is the exact wrapper that used to be the
//! `snowstorm` crate's `NoiseStream`.
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

const LENGTH_FIELD_LEN: usize = std::mem::size_of::<u16>();

#[derive(Debug)]
enum ReadState {
    ShuttingDown,
    Idle,
    ReadingLen(usize, [u8; 2]),
    ReadingMessage(usize),
    ServingPayload(usize),
}

#[derive(Debug)]
enum WriteState {
    ShuttingDown,
    Idle,
    WritingMessage(usize, usize),
}

#[pin_project]
pub struct NoiseStream<T> {
    #[pin]
    inner: T,

    transport: TransportState,
    read_state: ReadState,
    write_state: WriteState,
    write_clean_waker: Option<Waker>,

    read_message_buffer: Vec<u8>,
    read_payload_buffer: Vec<u8>,

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
                    read_state: ReadState::Idle,
                    write_state: WriteState::Idle,
                    write_clean_waker: None,
                    read_message_buffer: vec![0; MAX_MESSAGE_LEN],
                    read_payload_buffer: vec![0; MAX_MESSAGE_LEN],
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
            match state {
                WriteState::ShuttingDown => {
                    return Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
                }
                WriteState::Idle => {
                    let payload_len = buf.len().min(MAX_MESSAGE_LEN - TAG_LEN);
                    let buf = &buf[..payload_len];

                    #[expect(
                        unsafe_code,
                        reason = "buffer pre-allocated (and zeroed at construction); set_len avoids a
                                  per-record memset of the whole 64 KiB record buffer — upstream
                                  snowstorm pattern"
                    )]
                    // SAFETY: the buffer was allocated with exactly this
                    // capacity; the region is fully overwritten by
                    // `write_message` before it is exposed.
                    unsafe {
                        write_message_buffer.set_len(LENGTH_FIELD_LEN + MAX_MESSAGE_LEN);
                    }

                    let message_len = transport
                        .write_message(buf, &mut write_message_buffer[LENGTH_FIELD_LEN..])
                        .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                    // `message_len` is bounded by MAX_MESSAGE_LEN
                    // (u16::MAX), so the cast cannot truncate.
                    #[expect(
                        clippy::cast_possible_truncation,
                        reason = "message length is bounded by the u16 wire format"
                    )]
                    write_message_buffer[..LENGTH_FIELD_LEN]
                        .copy_from_slice(&(message_len as u16).to_le_bytes());
                    write_message_buffer.truncate(LENGTH_FIELD_LEN + message_len);
                    *state = WriteState::WritingMessage(0, payload_len);
                }
                WriteState::WritingMessage(start, payload_len) => {
                    let n = ready!(
                        Pin::new(&mut inner).poll_write(cx, &write_message_buffer[*start..])
                    )?;
                    *start += n;

                    if *start == write_message_buffer.len() {
                        let n = *payload_len;
                        *state = WriteState::Idle;
                        if let Some(waker) = this.write_clean_waker.take() {
                            waker.wake();
                        }
                        return Poll::Ready(Ok(n));
                    }
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
            WriteState::WritingMessage(..) => {}
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

        let read_message_buffer = this.read_message_buffer;
        let read_payload_buffer = this.read_payload_buffer;

        loop {
            match state {
                ReadState::ShuttingDown => {
                    return Poll::Ready(Ok(()));
                }
                ReadState::Idle => *state = ReadState::ReadingLen(0, [0; LENGTH_FIELD_LEN]),
                ReadState::ReadingLen(read_len, buf) => {
                    // Copy the 2-byte length slot: edition-2024 match
                    // ergonomics bind it by reference; the state machine
                    // needs the value (and to store it back).
                    let mut buf = *buf;
                    if *read_len == LENGTH_FIELD_LEN {
                        let message_len = u16::from_le_bytes(buf);

                        // Safety: This is safe because message_len <= MAX_MESSAGE_LEN
                        #[expect(
                            unsafe_code,
                            reason = "buffer pre-allocated (and zeroed at construction); set_len avoids a
                                      per-record memset of the whole 64 KiB record buffer — upstream
                                      snowstorm pattern"
                        )]
                        // SAFETY: `message_len` <= MAX_MESSAGE_LEN, the
                        // allocation size; the region is fully overwritten
                        // by the subsequent reads.
                        unsafe {
                            read_message_buffer.set_len(message_len as usize);
                        }
                        *state = ReadState::ReadingMessage(0);
                    } else {
                        let mut read_buf = ReadBuf::new(&mut buf);
                        read_buf.advance(*read_len);

                        ready!(Pin::new(&mut inner).poll_read(cx, &mut read_buf))?;
                        let n = read_buf.filled().len();
                        if n == 0 {
                            // EOF
                            *state = ReadState::ShuttingDown;
                        } else {
                            *state = ReadState::ReadingLen(n, buf);
                        }
                    }
                }
                ReadState::ReadingMessage(start) => {
                    if *start == read_message_buffer.len() {
                        // Safety: This is safe because this buffer is initialized with MAX_MESSAGE_LEN
                        #[expect(
                            unsafe_code,
                            reason = "buffer pre-allocated (and zeroed at construction); set_len avoids a
                                      per-record memset of the whole 64 KiB record buffer — upstream
                                      snowstorm pattern"
                        )]
                        // SAFETY: the buffer was allocated with exactly
                        // MAX_MESSAGE_LEN; `read_message` overwrites the
                        // region before it is served, then truncates.
                        unsafe {
                            read_payload_buffer.set_len(MAX_MESSAGE_LEN);
                        }

                        let n = transport
                            .read_message(read_message_buffer, read_payload_buffer)
                            .map_err(|e| std::io::Error::new(ErrorKind::InvalidData, e))?;
                        read_payload_buffer.truncate(n);
                        *state = ReadState::ServingPayload(0);
                    } else {
                        let mut read_buf = ReadBuf::new(&mut read_message_buffer[*start..]);

                        ready!(Pin::new(&mut inner).poll_read(cx, &mut read_buf))?;
                        let n = read_buf.filled().len();
                        if n == 0 {
                            // EOF
                            *state = ReadState::ShuttingDown;
                        } else {
                            *start += n;
                        }
                    }
                }
                ReadState::ServingPayload(start) => {
                    let read_buf_remaining = read_buf.remaining();
                    let buf_remaining = read_payload_buffer.len() - *start;

                    if buf_remaining <= read_buf_remaining {
                        read_buf.put_slice(&read_payload_buffer[*start..]);
                        *state = ReadState::Idle;
                    } else {
                        read_buf
                            .put_slice(&read_payload_buffer[*start..*start + read_buf_remaining]);
                        *start += read_buf_remaining;
                    }

                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}
