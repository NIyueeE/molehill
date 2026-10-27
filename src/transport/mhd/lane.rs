//! The async seam over one connection: shared state, and the two halves a
//! stream hands to its caller.
//!
//! The state machine in conn.rs is synchronous and owns no wakers. This module
//! is the other half of that decision: it holds the connection behind a mutex,
//! keeps the wakers of whoever is waiting for space or for data, and wakes the
//! pump when there is something new to send. Nothing here decides anything about
//! the protocol — it decides who to wake.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::conn::Conn;
use super::wire::ResetCode;
use crate::transport::health::SessionHealth;

/// Whoever is waiting on this connection, by reason.
#[derive(Debug, Default)]
struct Wakers {
    /// Readers with nothing to read yet.
    readers: Vec<Waker>,
    /// Writers the peer's credit is holding back.
    writers: Vec<Waker>,
}

/// One connection, shared by its streams and its pump.
#[derive(Debug)]
pub(crate) struct Shared {
    conn: Mutex<Conn>,
    wakers: Mutex<Wakers>,
    /// Tells the pump there is something new to send.
    ///
    /// A notification rather than a stored waker: the pump waits on a select
    /// between the socket and this, so one signal is all it needs and a wake-up
    /// that arrives between two passes is not lost.
    signal: tokio::sync::Notify,
    /// Tells a lane's owner that a stream arrived.
    ///
    /// Separate from [`Self::signal`] on purpose: one notification with two
    /// waiters lets the wrong one consume it, and a send that lost its wake-up
    /// to the accept loop waits for the keepalive timer (measured as a two
    /// second stall on every forwarded channel).
    streams: tokio::sync::Notify,
    /// Tells whoever waits on the connection that it ended: the pump, and the
    /// lane's server-side loop. A notification, for the same reason.
    ended: tokio::sync::Notify,
    /// The per-connection counters the status API reads.
    pub(crate) health: std::sync::Arc<SessionHealth>,
}

impl Shared {
    /// Wrap a connection.
    pub(crate) fn new(conn: Conn, health: std::sync::Arc<SessionHealth>) -> Arc<Self> {
        let conn = conn.with_health(std::sync::Arc::clone(&health));
        Arc::new(Self {
            conn: Mutex::new(conn),
            wakers: Mutex::new(Wakers::default()),
            signal: tokio::sync::Notify::new(),
            streams: tokio::sync::Notify::new(),
            ended: tokio::sync::Notify::new(),
            health,
        })
    }

    /// Run a closure with the connection held.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Conn) -> R) -> R {
        let mut conn = self.conn.lock();
        f(&mut conn)
    }

    /// End the connection: every stream ends, the pump stops after its last
    /// pass, and whoever waits on the lane is woken.
    pub(crate) fn close(&self, code: ResetCode) {
        let already = self.with(|conn| conn.closed());
        self.with(|conn| conn.close(code));
        if already.is_none() {
            tracing::debug!("mhd connection closed: {code:?}");
        }
        self.notify_closed();
    }

    /// Ask for the close, after whatever is already queued has been sent.
    ///
    /// A refusal is written and then the lane is told to die; closing at once
    /// would drop the refusal with the connection and leave the peer waiting on
    /// a request the server in fact answered.
    pub(crate) fn begin_close(&self, code: ResetCode) {
        self.with(|conn| conn.close_gracefully(code));
        self.wake_pump();
    }

    /// Complete a graceful close the pump has drained.
    pub(crate) fn finish_close(&self) {
        let code = self
            .with(|conn| conn.closed())
            .unwrap_or(ResetCode::Aborted);
        self.with(|conn| conn.close(code));
        tracing::debug!("mhd connection closed: {code:?}");
        self.notify_closed();
    }

    fn notify_closed(&self) {
        self.ended.notify_waiters();
        self.wake_readers();
        self.wake_writers();
        self.wake_pump();
    }

    /// Whether the connection ended.
    pub(crate) fn is_closed(&self) -> bool {
        self.with(|conn| conn.closed()).is_some()
    }

    /// Wait until the connection ends.
    pub(crate) async fn closed(&self) {
        loop {
            if self.is_closed() {
                return;
            }
            self.ended.notified().await;
        }
    }

    /// Record one datagram leaving the socket (the status API's send count).
    pub(crate) fn note_sent(&self) {
        self.health
            .datagrams_out
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Tell the pump there is something new to send.
    pub(crate) fn wake_pump(&self) {
        self.signal.notify_one();
    }

    /// Tell the lane's owner that a stream may have arrived.
    ///
    /// `notify_one` rather than `notify_waiters`: a wake-up that arrives
    /// between the owner's check and its wait must be remembered, or a stream
    /// waits for the next datagram.
    pub(crate) fn wake_streams(&self) {
        self.streams.notify_one();
    }

    /// Wait until a datagram may have opened a stream.
    pub(crate) async fn stream_added(&self) {
        self.streams.notified().await;
    }

    /// Wait until someone says there is something to send.
    pub(crate) async fn pump_signal(&self) {
        self.signal.notified().await;
    }

    /// Wake readers: bytes arrived, or the stream ended.
    pub(crate) fn wake_readers(&self) {
        let wakers = std::mem::take(&mut self.wakers.lock().readers);
        for waker in wakers {
            waker.wake();
        }
    }

    /// Wake writers: the peer's credit moved.
    pub(crate) fn wake_writers(&self) {
        let wakers = std::mem::take(&mut self.wakers.lock().writers);
        for waker in wakers {
            waker.wake();
        }
    }

    fn park_reader(&self, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        if !wakers.readers.iter().any(|w| w.will_wake(waker)) {
            wakers.readers.push(waker.clone());
        }
    }

    fn park_writer(&self, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        if !wakers.writers.iter().any(|w| w.will_wake(waker)) {
            wakers.writers.push(waker.clone());
        }
    }
}

/// One stream, as the layer above sees it.
#[derive(Debug)]
pub(crate) struct StreamIo {
    shared: Arc<Shared>,
    id: u32,
    /// Reused between polls so a read does not allocate.
    scratch: Vec<u8>,
}

impl StreamIo {
    /// Wrap one stream of a connection.
    pub(crate) fn new(shared: Arc<Shared>, id: u32) -> Self {
        Self {
            shared,
            id,
            scratch: Vec::new(),
        }
    }
}

impl AsyncRead for StreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        // A caller that asked for fewer bytes than had arrived left the rest
        // here; hand it out before reading the connection again.
        if !this.scratch.is_empty() {
            let room = buf.remaining().min(this.scratch.len());
            buf.put_slice(&this.scratch[..room]);
            this.scratch.drain(..room);
            return Poll::Ready(Ok(()));
        }
        let mut scratch = std::mem::take(&mut this.scratch);
        // Register before reading: a datagram that arrives between the read and
        // the park would otherwise find no waker and be lost, and the reader
        // would wait for the pump's next timer instead of the bytes.
        this.shared.park_reader(cx.waker());
        let (taken, ended, open) = this.shared.with(|conn| {
            let taken = conn.read(this.id, &mut scratch);
            (
                taken,
                conn.peer_finished(this.id) || conn.reset_reason(this.id).is_some(),
                conn.stream_open(this.id),
            )
        });
        if taken > 0 {
            let room = buf.remaining().min(scratch.len());
            buf.put_slice(&scratch[..room]);
            if room < scratch.len() {
                // The caller asked for less than arrived: keep the rest for the
                // next poll instead of dropping it.
                scratch.drain(..room);
                this.scratch = scratch;
            }
            return Poll::Ready(Ok(()));
        }
        this.scratch = scratch;
        if ended || !open {
            return Poll::Ready(Ok(()));
        }
        Poll::Pending
    }
}

impl AsyncWrite for StreamIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.shared.is_closed() {
            return Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)));
        }
        if !this.shared.with(|conn| conn.write_open(this.id)) {
            return Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Same rule as the read side: parked before the credit is re-read, so a
        // credit update between the two cannot be missed.
        this.shared.park_writer(cx.waker());
        let written = this.shared.with(|conn| conn.write(this.id, buf));
        if written > 0 {
            this.shared.wake_pump();
            return Poll::Ready(Ok(written));
        }
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // The pump owns the socket: a flushed stream is one whose bytes are
        // queued, which is what the write above guarantees.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.shared.with(|conn| conn.finish(this.id));
        this.shared.wake_pump();
        Poll::Ready(Ok(()))
    }
}

/// A dropped handle releases its stream.
///
/// The pool charges a stream to its lane until the handle dies, and the peer
/// must be told: without the reset it would keep writing into a stream nothing
/// reads, and its table would grow by one entry per visitor for the lane's whole
/// life.
impl Drop for StreamIo {
    fn drop(&mut self) {
        if !self.shared.is_closed() {
            self.shared.with(|conn| conn.close_stream(self.id));
            self.shared.wake_pump();
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
    use std::time::Instant;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Move everything both connections want to send, once, and deliver it.
    fn pump_once(a: &Arc<Shared>, b: &Arc<Shared>) {
        let now = Instant::now();
        let mut buf = [0u8; 2048];
        for (from, to) in [(a, b), (b, a)] {
            let mut packets = Vec::new();
            from.with(|conn| {
                while let Some(len) = conn.poll_transmit(now, &mut buf) {
                    packets.push(buf[..len].to_vec());
                }
            });
            for packet in &packets {
                to.with(|conn| {
                    let _ = conn.on_datagram(now, packet);
                });
            }
            to.wake_readers();
            to.wake_writers();
            from.wake_writers();
            from.wake_pump();
            to.wake_pump();
        }
    }

    /// The other end's stream id, once its first bytes have been seen.
    fn accepted(shared: &Arc<Shared>) -> Option<u32> {
        shared.with(|conn| conn.accept().map(|handle| handle.id))
    }

    /// The whole seam in one test: a task writes through the async trait, the
    /// pump moves datagrams, and another task reads the bytes back out of the
    /// peer's stream. Nothing here touches a socket.
    #[tokio::test]
    async fn bytes_written_through_the_trait_arrive_on_the_peer() {
        let dialer = Shared::new(
            Conn::new(11, true),
            crate::transport::health::SessionHealth::new(),
        );
        let acceptor = Shared::new(
            Conn::new(11, false),
            crate::transport::health::SessionHealth::new(),
        );

        let pump = {
            let dialer = Arc::clone(&dialer);
            let acceptor = Arc::clone(&acceptor);
            tokio::spawn(async move {
                loop {
                    pump_once(&dialer, &acceptor);
                    tokio::task::yield_now().await;
                }
            })
        };

        let stream = StreamIo::new(Arc::clone(&dialer), dialer.with(|conn| conn.open().id));
        let writer = tokio::spawn(async move {
            let mut stream = stream;
            stream.write_all(b"the quick brown fox").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        // Wait for the stream to be named on the peer, then read it whole.
        let mut peer_id = None;
        for _ in 0..1000 {
            if let Some(id) = accepted(&acceptor) {
                peer_id = Some(id);
                break;
            }
            tokio::task::yield_now().await;
        }
        let Some(peer_id) = peer_id else {
            unreachable!("the peer must see the stream")
        };
        let mut reader = StreamIo::new(Arc::clone(&acceptor), peer_id);
        let mut got = Vec::new();
        let Ok(Ok(_)) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reader.read_to_end(&mut got),
        )
        .await
        else {
            unreachable!("the read must finish and succeed")
        };

        writer.await.unwrap();
        pump.abort();
        assert_eq!(got, b"the quick brown fox");
    }
}
