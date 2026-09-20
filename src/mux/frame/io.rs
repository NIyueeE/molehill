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
use futures::{prelude::*, ready};
use std::{
    fmt, io,
    pin::Pin,
    task::{Context, Poll},
};

/// Maximum Yamux frame body length
///
/// Limits the amount of bytes a remote can cause the local node to allocate at once when reading.
///
/// Chosen based on intuition in past iterations.
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
        buffer: Vec<u8>,
        offset: usize,
    },
    Body {
        buffer: Vec<u8>,
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

impl<T: AsyncRead + AsyncWrite + Unpin> Sink<Frame<()>> for Io<T> {
    type Error = io::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
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

    fn start_send(self: Pin<&mut Self>, f: Frame<()>) -> Result<(), Self::Error> {
        let header = header::encode(&f.header);
        let buffer = f.body;
        self.get_mut().write_state = WriteState::Header {
            header,
            buffer,
            offset: 0,
        };
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        ready!(this.poll_ready_unpin(cx))?;
        Pin::new(&mut this.socket).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let this = Pin::into_inner(self);
        ready!(this.poll_ready_unpin(cx))?;
        Pin::new(&mut this.socket).poll_close(cx)
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
        buffer: Vec<u8>,
    },
}

impl<T: AsyncRead + AsyncWrite + Unpin> Stream for Io<T> {
    type Item = Result<Frame<()>, FrameDecodeError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
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
                            buffer: vec![0; body_len],
                        };

                        continue;
                    }

                    let buf = &mut buffer[*offset..header::HEADER_SIZE];
                    match ready!(Pin::new(&mut this.socket).poll_read(cx, buf))? {
                        0 => {
                            if *offset == 0 {
                                return Poll::Ready(None);
                            }
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)));
                        }
                        n => *offset += n,
                    }
                }
                ReadState::Body {
                    ref header,
                    ref mut offset,
                    ref mut buffer,
                } => {
                    let body_len = header.len().val() as usize;

                    if *offset == body_len {
                        let h = header.clone();
                        let v = std::mem::take(buffer);
                        this.read_state = ReadState::Init;
                        return Poll::Ready(Some(Ok(Frame { header: h, body: v })));
                    }

                    let buf = &mut buffer[*offset..body_len];
                    match ready!(Pin::new(&mut this.socket).poll_read(cx, buf))? {
                        0 => {
                            let e = FrameDecodeError::Io(io::ErrorKind::UnexpectedEof.into());
                            return Poll::Ready(Some(Err(e)));
                        }
                        n => *offset += n,
                    }
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
