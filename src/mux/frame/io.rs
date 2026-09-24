// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use super::{
    Frame,
    header::{self, HeaderDecodeError},
};
use crate::mux::connection::Id;
use bytes::{Bytes, BytesMut};
use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll, ready},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Maximum Yamux frame body length
///
/// Limits the amount of bytes a remote can cause the local node to allocate at once when reading.
///
/// Chosen based on intuition in past iterations.
/// Bodies up to this size are written together with their header in one
/// buffer (see [`Io::start_frame`]).
pub(crate) const COALESCE_BODY_MAX: usize = 512;

const MAX_FRAME_BODY_LEN: usize = crate::mux::MIB;

/// A [`Stream`] and writer of [`Frame`] values.
#[derive(Debug)]
pub(crate) struct Io<T> {
    id: Id,
    socket: T,
    read_state: ReadState,
    write_state: WriteState,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    pub(crate) fn new(id: Id, socket: T) -> Self {
        Io {
            id,
            socket,
            read_state: ReadState::Init,
            write_state: WriteState::Init,
        }
    }
}

/// The stages of writing a new `Frame`.
enum WriteState {
    Init,
    Header {
        header: [u8; header::HEADER_SIZE],
        buffer: Bytes,
        offset: usize,
    },
    Body {
        buffer: Bytes,
        offset: usize,
    },
    Poisoned,
}

impl fmt::Debug for WriteState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            WriteState::Init => f.write_str("(WriteState::Init)"),
            WriteState::Header { offset, .. } => {
                write!(f, "(WriteState::Header (offset {offset}))")
            }
            WriteState::Body { offset, buffer } => {
                write!(
                    f,
                    "(WriteState::Body (offset {}) (buffer-len {}))",
                    offset,
                    buffer.len()
                )
            }
            WriteState::Poisoned => f.write_str("(WriteState::Poisoned)"),
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    /// Whether the frame writer is idle and can accept a new frame.
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self.write_state, WriteState::Init)
    }

    /// Queue a frame for writing. The caller checks [`Io::is_idle`] first
    /// and drives the writer with [`Io::poll_flush`] afterwards.
    pub(crate) fn start_frame(&mut self, f: Frame<()>) {
        use std::sync::atomic::Ordering::Relaxed;
        crate::mux::FRAMES_WRITTEN.fetch_add(1, Relaxed);
        crate::mux::FRAME_BYTES.fetch_add(f.body.len() as u64, Relaxed);
        let header = header::encode(&f.header);
        let buffer = f.body;
        self.write_state = if buffer.len() <= COALESCE_BODY_MAX {
            // One buffer, one write: header first, then the body. The copy
            // is trivial at this size and it halves the write calls for the
            // control frames (SYN/ACK/FIN/window update/ping) that dominate
            // light-load and churn traffic. Larger bodies keep the two-phase
            // write so the payload is never copied twice.
            let mut combined = BytesMut::with_capacity(header.len() + buffer.len());
            combined.extend_from_slice(&header);
            combined.extend_from_slice(&buffer);
            WriteState::Body {
                buffer: combined.freeze(),
                offset: 0,
            }
        } else {
            WriteState::Header {
                header,
                buffer,
                offset: 0,
            }
        };
    }

    /// Drive the frame writer to completion, then flush the socket.
    pub(crate) fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = Pin::into_inner(self);
        ready!(Pin::new(&mut *this).drive_write(cx))?;
        Pin::new(&mut this.socket).poll_flush(cx)
    }

    /// Run the frame-writer state machine until it is idle or pending.
    fn drive_write(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = Pin::into_inner(self);
        loop {
            tracing::trace!("{}: write: {:?}", this.id, this.write_state);
            match &mut this.write_state {
                WriteState::Init => return Poll::Ready(Ok(())),
                WriteState::Header {
                    header,
                    buffer,
                    offset,
                } => match Pin::new(&mut this.socket).poll_write(cx, &header[*offset..]) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Ready(Ok(n)) => {
                        if n == 0 {
                            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                        }
                        *offset += n;

                        if *offset > header.len() {
                            let err = io::Error::other(format!(
                                "Writer header returned invalid write count n={n}: {offset} > {} ",
                                header.len(),
                            ));

                            this.write_state = WriteState::Poisoned;

                            return Poll::Ready(Err(err));
                        }

                        if *offset == header.len() {
                            if buffer.is_empty() {
                                this.write_state = WriteState::Init;
                            } else {
                                let buffer = std::mem::take(buffer);
                                this.write_state = WriteState::Body { buffer, offset: 0 };
                            }
                        }
                    }
                },
                WriteState::Body { buffer, offset } => {
                    match Pin::new(&mut this.socket).poll_write(cx, &buffer[*offset..]) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                        Poll::Ready(Ok(n)) => {
                            if n == 0 {
                                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                            }
                            *offset += n;

                            if *offset > buffer.len() {
                                let err = io::Error::other(format!(
                                    "Writer body returned invalid write count n={n}: {offset} > {} ",
                                    buffer.len(),
                                ));

                                this.write_state = WriteState::Poisoned;

                                return Poll::Ready(Err(err));
                            }

                            if *offset == buffer.len() {
                                this.write_state = WriteState::Init;
                            }
                        }
                    }
                }
                WriteState::Poisoned => {
                    return Poll::Ready(Err(io::Error::other(
                        "Sink is in poisoned state due to previous write error",
                    )));
                }
            }
        }
    }
}

/// The stages of reading a new `Frame`.
enum ReadState {
    /// Initial reading state.
    Init,
    /// Reading the frame header.
    Header {
        offset: usize,
        buffer: [u8; header::HEADER_SIZE],
    },
    /// Reading the frame body.
    Body {
        header: header::Header<()>,
        offset: usize,
        buffer: BytesMut,
    },
}

impl<T: AsyncRead + AsyncWrite + Unpin> Io<T> {
    /// Read the next frame off the socket.
    pub(crate) fn poll_next_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<()>, FrameDecodeError>>> {
        let this = &mut *self;
        loop {
            tracing::trace!("{}: read: {:?}", this.id, this.read_state);
            match this.read_state {
                ReadState::Init => {
                    this.read_state = ReadState::Header {
                        offset: 0,
                        buffer: [0; header::HEADER_SIZE],
                    };
                }
                ReadState::Header {
                    ref mut offset,
                    ref mut buffer,
                } => {
                    if *offset == header::HEADER_SIZE {
                        let header = match header::decode(buffer) {
                            Ok(hd) => hd,
                            Err(e) => return Poll::Ready(Some(Err(e.into()))),
                        };

                        tracing::trace!("{}: read: {}", this.id, header);

                        if header.tag() != header::Tag::Data {
                            this.read_state = ReadState::Init;
                            return Poll::Ready(Some(Ok(Frame::new(header))));
                        }

                        let body_len = header.len().val() as usize;

                        if body_len > MAX_FRAME_BODY_LEN {
                            return Poll::Ready(Some(Err(FrameDecodeError::FrameTooLarge(
                                body_len,
                            ))));
                        }

                        this.read_state = ReadState::Body {
                            header,
                            offset: 0,
                            buffer: BytesMut::zeroed(body_len),
                        };

                        continue;
                    }

                    let buf = &mut buffer[*offset..header::HEADER_SIZE];
                    let mut read_buf = ReadBuf::new(buf);
                    ready!(Pin::new(&mut this.socket).poll_read(cx, &mut read_buf))?;
                    let n = read_buf.filled().len();
                    if n == 0 {
                        if *offset == 0 {
                            return Poll::Ready(None);
                        }
                        let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                        return Poll::Ready(Some(Err(e)));
                    }
                    *offset += n;
                }
                ReadState::Body {
                    ref header,
                    ref mut offset,
                    ref mut buffer,
                } => {
                    let body_len = header.len().val() as usize;

                    if *offset == body_len {
                        let h = header.clone();
                        // The receive buffer freezes in place: the frame
                        // body moves to the stream buffer by ownership.
                        let v = std::mem::take(buffer).freeze();
                        this.read_state = ReadState::Init;
                        crate::mux::FRAMES_READ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        crate::mux::FRAME_BYTES
                            .fetch_add(v.len() as u64, std::sync::atomic::Ordering::Relaxed);
                        return Poll::Ready(Some(Ok(Frame { header: h, body: v })));
                    }

                    let buf = &mut buffer[*offset..body_len];
                    let mut read_buf = ReadBuf::new(buf);
                    ready!(Pin::new(&mut this.socket).poll_read(cx, &mut read_buf))?;
                    let n = read_buf.filled().len();
                    if n == 0 {
                        let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                        return Poll::Ready(Some(Err(e)));
                    }
                    *offset += n;
                }
            }
        }
    }
}

impl fmt::Debug for ReadState {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ReadState::Init => f.write_str("(ReadState::Init)"),
            ReadState::Header { offset, .. } => {
                write!(f, "(ReadState::Header (offset {offset}))")
            }
            ReadState::Body {
                header,
                offset,
                buffer,
            } => {
                write!(
                    f,
                    "(ReadState::Body (header {}) (offset {}) (buffer-len {}))",
                    header,
                    offset,
                    buffer.len()
                )
            }
        }
    }
}

/// Possible errors while decoding a message frame.
#[non_exhaustive]
#[derive(Debug)]
pub enum FrameDecodeError {
    /// An I/O error.
    Io(io::Error),
    /// Decoding the frame header failed.
    Header(HeaderDecodeError),
    /// A data frame body length is larger than the configured maximum.
    FrameTooLarge(usize),
}

impl std::fmt::Display for FrameDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            FrameDecodeError::Io(e) => write!(f, "i/o error: {e}"),
            FrameDecodeError::Header(e) => write!(f, "decode error: {e}"),
            FrameDecodeError::FrameTooLarge(n) => write!(f, "frame body is too large ({n})"),
        }
    }
}

impl std::error::Error for FrameDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FrameDecodeError::Io(e) => Some(e),
            FrameDecodeError::Header(e) => Some(e),
            FrameDecodeError::FrameTooLarge(_) => None,
        }
    }
}

impl From<std::io::Error> for FrameDecodeError {
    fn from(e: std::io::Error) -> Self {
        FrameDecodeError::Io(e)
    }
}

impl From<HeaderDecodeError> for FrameDecodeError {
    fn from(e: HeaderDecodeError) -> Self {
        FrameDecodeError::Header(e)
    }
}
