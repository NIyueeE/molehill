//! Streams over one engine connection.
//!
//! quinn-proto hands out send/recv handles that borrow the connection and has
//! no async at all: it reports readable/writable events and expects the owner
//! to poll. So the connection lives behind a mutex, a stream handle is an
//! (`Arc<Shared>`, `StreamId`) pair, and a blocked direction leaves one waker that
//! the task fires when the engine reports the stream readable, writable or
//! finished.
//!
//! Every stream opens with a selector byte, exactly like a TCP data channel:
//! `PLAIN_SELECTOR` for bytes as they are, `NOISE_SELECTOR` when the stream is
//! wrapped in the per-stream record layer (`record.rs`). The key comes from the
//! connection's handshake hash, so the selector is what tells the accepting
//! side which one it is looking at.

use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use parking_lot::Mutex;
use quinn_proto::{Connection, Dir, ReadError, ReadableError, StreamId, VarInt, WriteError};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

use super::record::{MAX_PLAINTEXT, RecordKey};

/// Whether this handle opened the stream or accepted the peer's.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Initiator,
    Acceptor,
}

/// Which end of a stream is waiting, so one stream can park two wakers.
///
/// A direct channel reads and writes at the same time; with a single slot per
/// stream the reader's park overwrites the writer's and the writer is never
/// woken again, which stalls the channel after its first exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Wait {
    Read,
    Write,
}

/// The connection a task and its stream handles share.
pub(crate) struct Shared {
    pub(crate) connection: Mutex<Connection>,
    /// The handshake hash the record keys derive from, once the engine
    /// reports the handshake is done.
    hash: Mutex<Option<Vec<u8>>>,
    /// At most one waker per stream: a writer waiting for credit, or a reader
    /// waiting for bytes.
    /// One waker per stream **and direction**: a direct channel reads and writes
    /// at the same time, so a single slot per stream lets the reader's park
    /// overwrite the writer's and the writer is never woken again.
    wakers: Mutex<HashMap<(StreamId, Wait), Waker>>,
    /// Set once the connection is gone, so every pending call reports it.
    closed: Mutex<bool>,
    /// Told whenever a write put bytes into the engine: the task owns the
    /// socket, so nothing else would wake it.
    notify: Arc<Notify>,
}

impl Shared {
    pub(crate) fn new(connection: Connection, notify: Arc<Notify>) -> Arc<Self> {
        Arc::new(Shared {
            connection: Mutex::new(connection),
            hash: Mutex::new(None),
            wakers: Mutex::new(HashMap::new()),
            closed: Mutex::new(false),
            notify,
        })
    }

    /// Record the handshake hash the record layer derives from.
    pub(crate) fn set_hash(&self, hash: Vec<u8>) {
        *self.hash.lock() = Some(hash);
    }

    fn hash(&self) -> Option<Vec<u8>> {
        self.hash.lock().clone()
    }

    /// Wake whatever was waiting on this stream.
    pub(crate) fn wake(&self, id: StreamId, wait: Wait) {
        if let Some(waker) = self.wakers.lock().remove(&(id, wait)) {
            waker.wake();
        }
    }

    /// Mark the connection gone and release everyone waiting on it.
    ///
    /// `closed` is held across the drain so a parker cannot slip its waker in
    /// after the drain: `park` takes the same lock and reports the connection
    /// gone to a caller that arrives too late.
    pub(crate) fn close(&self) {
        let mut closed = self.closed.lock();
        *closed = true;
        let wakers: Vec<Waker> = self.wakers.lock().drain().map(|(_, waker)| waker).collect();
        drop(closed);
        for waker in wakers {
            waker.wake();
        }
    }

    /// Register a waker for one direction of one stream.
    ///
    /// Callers hold the connection lock across this, which is what closes the
    /// lost-wakeup window: the task that fires the waker needs that lock to
    /// observe the engine's event, so it cannot fire between the failed poll
    /// and this registration.
    fn park(&self, id: StreamId, wait: Wait, waker: &Waker) {
        let closed = self.closed.lock();
        if *closed {
            drop(closed);
            waker.wake_by_ref();
            return;
        }
        self.wakers.lock().insert((id, wait), waker.clone());
    }

    fn is_closed(&self) -> bool {
        *self.closed.lock()
    }

    fn poke(&self) {
        self.notify.notify_one();
    }
}

/// The write half's own state: what still has to reach the engine, and
/// whether the selector byte has gone out.
struct WriteHalf {
    key: Option<RecordKey>,
    pending: Vec<u8>,
    selector_pending: bool,
}

/// The read half's: how much framed bytes we hold, and whether the peer's
/// selector byte has been consumed.
struct ReadHalf {
    key: Option<RecordKey>,
    selector_pending: bool,
    /// Bytes read from the engine that are not plaintext yet.
    raw: Vec<u8>,
    /// Plaintext waiting for the caller.
    plain: Vec<u8>,
}

/// One bidirectional stream, as the carrier's seam sees it.
pub(crate) struct QuicStream {
    shared: Arc<Shared>,
    id: StreamId,
    role: Role,
    write: WriteHalf,
    read: ReadHalf,
    finished: bool,
}

impl QuicStream {
    /// A stream we opened. The selector byte goes out with the first write.
    ///
    /// Asking for encryption before the handshake hash exists is an error
    /// rather than a silent plaintext stream: the caller asked for a
    /// guarantee, and downgrading it quietly is the one thing this layer must
    /// never do.
    pub(crate) fn initiator(shared: Arc<Shared>, id: StreamId, encrypt: bool) -> io::Result<Self> {
        let (write_key, read_key) = if encrypt {
            match (
                Self::key(&shared, id, Role::Initiator),
                Self::key(&shared, id, Role::Acceptor),
            ) {
                (Some(write), Some(read)) => (Some(write), Some(read)),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "the handshake hash is not available yet",
                    ));
                }
            }
        } else {
            (None, None)
        };
        Ok(QuicStream {
            shared,
            id,
            role: Role::Initiator,
            write: WriteHalf {
                key: write_key,
                pending: Vec::new(),
                selector_pending: true,
            },
            read: ReadHalf {
                key: read_key,
                selector_pending: false,
                raw: Vec::new(),
                plain: Vec::new(),
            },
            finished: false,
        })
    }

    /// A stream the peer opened: its protection class arrives as the selector
    /// byte the peer writes first.
    pub(crate) fn acceptor(shared: Arc<Shared>, id: StreamId) -> Self {
        QuicStream {
            shared,
            id,
            role: Role::Acceptor,
            write: WriteHalf {
                key: None,
                pending: Vec::new(),
                selector_pending: false,
            },
            read: ReadHalf {
                key: None,
                selector_pending: true,
                raw: Vec::new(),
                plain: Vec::new(),
            },
            finished: false,
        }
    }

    /// The record key for one direction: the flag names the sender, so both
    /// ends derive the same key for the same direction.
    fn key(shared: &Shared, id: StreamId, sender: Role) -> Option<RecordKey> {
        let hash = shared.hash()?;
        Some(RecordKey::derive(
            &hash,
            u64::from(id.index()),
            sender == Role::Initiator,
        ))
    }

    fn selector(&self) -> u8 {
        if self.write.key.is_some() {
            crate::protocol::NOISE_SELECTOR
        } else {
            crate::protocol::PLAIN_SELECTOR
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
        if this.shared.is_closed() {
            return Poll::Ready(Err(gone()));
        }
        let mut connection = this.shared.connection.lock();

        // Flush whatever a previous call could not hand over.
        if !this.write.pending.is_empty() {
            match flush(&mut connection, this.id, &mut this.write.pending) {
                Ok(_) if this.write.pending.is_empty() => {}
                Ok(_) => {
                    this.shared.park(this.id, Wait::Write, cx.waker());
                    return Poll::Pending;
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }

        // The selector byte rides with the first payload, so a caller never
        // sees a zero-length write.
        let mut frame = Vec::new();
        if this.write.selector_pending {
            frame.push(this.selector());
            this.write.selector_pending = false;
        }
        let take = buf.len().min(MAX_PLAINTEXT);
        if take > 0 {
            let chunk = &buf[..take];
            match this.write.key.as_mut() {
                Some(key) => frame.extend_from_slice(&key.seal(chunk)),
                None => frame.extend_from_slice(chunk),
            }
        }
        this.write.pending = frame;
        let consumed = take;

        match flush(&mut connection, this.id, &mut this.write.pending) {
            Ok(_) => {
                drop(connection);
                this.shared.poke();
                Poll::Ready(Ok(consumed))
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    /// Push whatever a previous write left behind into the engine.
    ///
    /// A short write can leave bytes in `pending` after `poll_write` has told
    /// its caller they were consumed. Without this, a caller that writes once
    /// and then waits for an answer (a lane's request/response traffic) would
    /// hold those bytes until its next write, which may never come.
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.write.pending.is_empty() {
            return Poll::Ready(Ok(()));
        }
        if this.shared.is_closed() {
            return Poll::Ready(Err(gone()));
        }
        let mut connection = this.shared.connection.lock();
        match flush(&mut connection, this.id, &mut this.write.pending) {
            Ok(_) if this.write.pending.is_empty() => {
                drop(connection);
                this.shared.poke();
                Poll::Ready(Ok(()))
            }
            Ok(_) => {
                this.shared.park(this.id, Wait::Write, cx.waker());
                Poll::Pending
            }
            Err(error) => Poll::Ready(Err(error)),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(Ok(()));
        }
        if this.shared.is_closed() {
            return Poll::Ready(Err(gone()));
        }
        let mut connection = this.shared.connection.lock();
        // FIN after the bytes: finishing a stream with an unsent tail would
        // discard exactly the last frame a lane wrote.
        if !this.write.pending.is_empty() {
            match flush(&mut connection, this.id, &mut this.write.pending) {
                Ok(_) if this.write.pending.is_empty() => {}
                Ok(_) => {
                    this.shared.park(this.id, Wait::Write, cx.waker());
                    return Poll::Pending;
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
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

/// Push as much of the pending buffer into the engine as it will take.
fn flush(connection: &mut Connection, id: StreamId, pending: &mut Vec<u8>) -> io::Result<usize> {
    if pending.is_empty() {
        return Ok(0);
    }
    match connection.send_stream(id).write(pending) {
        Ok(written) => {
            pending.drain(..written);
            Ok(written)
        }
        Err(WriteError::Blocked) => Ok(0),
        Err(WriteError::Stopped(code)) => Err(reset(code)),
        Err(WriteError::ClosedStream) => Err(gone()),
    }
}

impl AsyncRead for QuicStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.shared.is_closed() {
            return Poll::Ready(Err(gone()));
        }

        if !this.read.plain.is_empty() {
            let take = buf.remaining().min(this.read.plain.len());
            buf.put_slice(&this.read.plain[..take]);
            this.read.plain.drain(..take);
            return Poll::Ready(Ok(()));
        }

        // The connection lock is held until this call has either produced
        // bytes or parked its reader. That is not a micro-optimisation: the
        // task that raises `Readable` needs this lock, so holding it across
        // the park closes the window in which a wakeup could be delivered to
        // nobody between the failed read and the registration.
        let mut connection = this.shared.connection.lock();
        let mut eof = false;
        {
            let mut recv = connection.recv_stream(this.id);
            // Ordered, always: this handle exposes a byte stream, and an
            // unordered read hands back segments whose offsets the caller would
            // have to reassemble. Appending them in arrival order is what
            // corrupted a lane as soon as a packet overtook a retransmission.
            let mut chunks = match recv.read(true) {
                Ok(chunks) => chunks,
                Err(ReadableError::IllegalOrderedRead | ReadableError::ClosedStream) => {
                    return Poll::Ready(Err(gone()));
                }
            };
            let scratch = vec![0u8; MAX_PLAINTEXT + 2 + super::record::TAG_LEN];
            loop {
                match chunks.next(scratch.len()) {
                    Ok(Some(chunk)) => this.read.raw.extend_from_slice(&chunk.bytes),
                    Ok(None) => {
                        eof = true;
                        break;
                    }
                    Err(ReadError::Blocked) => break,
                    Err(ReadError::Reset(code)) => return Poll::Ready(Err(reset(code))),
                }
            }
        }

        // The peer's selector byte decides this stream's protection class.
        if this.read.selector_pending {
            if this.read.raw.is_empty() {
                if eof {
                    return Poll::Ready(Ok(()));
                }
                this.shared.park(this.id, Wait::Read, cx.waker());
                return Poll::Pending;
            }
            let selector = this.read.raw.remove(0);
            this.read.selector_pending = false;
            if selector == crate::protocol::NOISE_SELECTOR {
                this.read.key = Self::key(&this.shared, this.id, Role::Initiator);
                this.write.key = Self::key(&this.shared, this.id, Role::Acceptor);
                if this.read.key.is_none() {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "no handshake hash for an encrypted stream",
                    )));
                }
            }
        }

        // De-frame what we have.
        if this.read.key.is_none() {
            this.read.plain.append(&mut this.read.raw);
        } else {
            while let Some(frame_len) = frame_len(&this.read.raw) {
                if this.read.raw.len() < frame_len {
                    break;
                }
                let frame: Vec<u8> = this.read.raw.drain(..frame_len).collect();
                match this.read.key.as_mut().and_then(|key| key.open(&frame)) {
                    Some(plaintext) => this.read.plain.extend_from_slice(&plaintext),
                    None => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "a record frame did not authenticate",
                        )));
                    }
                }
            }
        }

        if !this.read.plain.is_empty() {
            let take = buf.remaining().min(this.read.plain.len());
            buf.put_slice(&this.read.plain[..take]);
            this.read.plain.drain(..take);
            return Poll::Ready(Ok(()));
        }
        if eof {
            return Poll::Ready(Ok(()));
        }
        this.shared.park(this.id, Wait::Read, cx.waker());
        Poll::Pending
    }
}

/// The length of the frame at the head of the buffer, if its prefix is there.
fn frame_len(raw: &[u8]) -> Option<usize> {
    if raw.len() < 2 {
        return None;
    }
    let body = usize::from(u16::from_be_bytes([raw[0], raw[1]]));
    Some(2 + body)
}

/// Open a stream on this connection, if the peer allows one more.
pub(crate) fn open(shared: &Arc<Shared>, encrypt: bool) -> io::Result<QuicStream> {
    let mut connection = shared.connection.lock();
    let id = connection
        .streams()
        .open(Dir::Bi)
        .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no stream available"))?;
    drop(connection);
    QuicStream::initiator(Arc::clone(shared), id, encrypt)
}

/// The peer's next stream, if it opened one.
pub(crate) fn accept(shared: &Arc<Shared>) -> Option<QuicStream> {
    let mut connection = shared.connection.lock();
    let id = connection.streams().accept(Dir::Bi)?;
    drop(connection);
    Some(QuicStream::acceptor(Arc::clone(shared), id))
}
