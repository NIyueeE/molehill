//! Streams over one engine connection.
//!
//! `quinn-proto` hands out `SendStream`/`RecvStream` that borrow the connection,
//! and it has no async at all: it reports `Readable`/`Writable` events and
//! expects the owner to poll. So the connection lives behind a mutex, a stream
//! handle is an `(Arc<Shared>, StreamId)` pair, and a blocked direction leaves
//! one waker that the task fires when the engine reports the stream readable,
//! writable or finished. That mirrors `quinn` itself and keeps backpressure
//! honest: a write either enters the engine now or waits for a writable event.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use parking_lot::Mutex;
use quinn_proto::{Connection, Dir, ReadError, ReadableError, StreamId, VarInt, WriteError};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

/// The connection a task and its stream handles share.
pub(crate) struct Shared {
    pub(crate) connection: Mutex<Connection>,
    /// At most one waker per stream: a writer waiting for credit, or a reader
    /// waiting for bytes.
    wakers: Mutex<HashMap<StreamId, Waker>>,
    /// Set once the connection is gone, so every pending call reports it
    /// instead of waiting for an event that will never come.
    closed: Mutex<bool>,
    /// Told whenever a write put bytes into the engine: the task owns the
    /// socket, so without this it would keep sleeping until some *incoming*
    /// datagram happened to wake it.
    notify: Arc<Notify>,
}

impl Shared {
    pub(crate) fn new(connection: Connection, notify: Arc<Notify>) -> Arc<Self> {
        Arc::new(Shared {
            connection: Mutex::new(connection),
            wakers: Mutex::new(HashMap::new()),
            closed: Mutex::new(false),
            notify,
        })
    }

    /// Ask the connection task to poll the engine again.
    fn poke(&self) {
        self.notify.notify_one();
    }

    /// Wake whatever was waiting on this stream.
    pub(crate) fn wake(&self, id: StreamId) {
        if let Some(waker) = self.wakers.lock().remove(&id) {
            waker.wake();
        }
    }

    /// Mark the connection gone and release everyone waiting on it.
    pub(crate) fn close(&self) {
        *self.closed.lock() = true;
        let wakers: Vec<Waker> = self.wakers.lock().drain().map(|(_, waker)| waker).collect();
        for waker in wakers {
            waker.wake();
        }
    }

    fn park(&self, id: StreamId, waker: &Waker) {
        self.wakers.lock().insert(id, waker.clone());
    }

    fn is_closed(&self) -> bool {
        *self.closed.lock()
    }
}

/// One bidirectional stream, as the carrier's seam sees it.
pub(crate) struct QuicStream {
    shared: Arc<Shared>,
    id: StreamId,
    finished: bool,
}

impl QuicStream {
    pub(crate) fn new(shared: Arc<Shared>, id: StreamId) -> Self {
        QuicStream {
            shared,
            id,
            finished: false,
        }
    }
}

fn gone() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "the carrier connection is gone",
    )
}

fn reset(code: VarInt) -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        format!("the peer reset the stream ({code})"),
    )
}

impl AsyncWrite for QuicStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let mut connection = this.shared.connection.lock();
        if this.shared.is_closed() {
            return Poll::Ready(Err(gone()));
        }
        match connection.send_stream(this.id).write(buf) {
            Ok(written) => {
                drop(connection);
                this.shared.poke();
                Poll::Ready(Ok(written))
            }
            Err(WriteError::Blocked) => {
                this.shared.park(this.id, cx.waker());
                Poll::Pending
            }
            Err(WriteError::Stopped(code)) => Poll::Ready(Err(reset(code))),
            Err(WriteError::ClosedStream) => Poll::Ready(Err(gone())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // A write enters the engine's own queue; there is no buffer of ours to
        // flush.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(Ok(()));
        }
        let mut connection = this.shared.connection.lock();
        match connection.send_stream(this.id).finish() {
            Ok(()) | Err(quinn_proto::FinishError::ClosedStream) => {
                this.finished = true;
                drop(connection);
                this.shared.poke();
                Poll::Ready(Ok(()))
            }
            Err(quinn_proto::FinishError::Stopped(code)) => Poll::Ready(Err(reset(code))),
        }
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let mut connection = this.shared.connection.lock();
        if this.shared.is_closed() {
            return Poll::Ready(Err(gone()));
        }
        let mut recv = connection.recv_stream(this.id);
        let mut chunks = match recv.read(false) {
            Ok(chunks) => chunks,
            // Unordered reads are what every call asks for, so the ordered
            // variant cannot occur.
            Err(ReadableError::IllegalOrderedRead) => return Poll::Ready(Err(gone())),
            Err(ReadableError::ClosedStream) => return Poll::Ready(Err(gone())),
        };
        match chunks.next(buf.remaining()) {
            Ok(Some(chunk)) => {
                buf.put_slice(&chunk.bytes);
                Poll::Ready(Ok(()))
            }
            // The stream is finished and everything was read: an empty read is
            // how a stream says end-of-file.
            Ok(None) => Poll::Ready(Ok(())),
            Err(ReadError::Blocked) => {
                this.shared.park(this.id, cx.waker());
                Poll::Pending
            }
            Err(ReadError::Reset(code)) => Poll::Ready(Err(reset(code))),
        }
    }
}

/// Open a stream on this connection, if the peer allows one more.
pub(crate) fn open(shared: &Arc<Shared>) -> io::Result<QuicStream> {
    let mut connection = shared.connection.lock();
    let id = connection
        .streams()
        .open(Dir::Bi)
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no stream available"))?;
    drop(connection);
    Ok(QuicStream::new(Arc::clone(shared), id))
}

/// The peer's next stream, if it opened one.
pub(crate) fn accept(shared: &Arc<Shared>) -> Option<QuicStream> {
    let mut connection = shared.connection.lock();
    let id = connection.streams().accept(Dir::Bi)?;
    drop(connection);
    Some(QuicStream::new(Arc::clone(shared), id))
}
