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
    ///
    /// The mutex is **blocking** (`parking_lot`), so a caller that waits for it
    /// parks an OS thread — and every other task that thread was running. That is
    /// the property behind the measurement this method now reports: with sixteen
    /// runtime workers the pairing budget expired **four** times against **once**
    /// with four, which is contention getting worse with more threads, not
    /// starvation getting better. A hundred milliseconds of waiting is fifty times
    /// a healthy acquisition, so anything past it is worth one line.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Conn) -> R) -> R {
        if !crate::transport::stats::enabled() {
            let mut conn = self.conn.lock();
            return f(&mut conn);
        }
        let asked = std::time::Instant::now();
        let mut conn = self.conn.lock();
        let waited = asked.elapsed();
        if waited >= std::time::Duration::from_millis(100) {
            tracing::debug!(
                waited_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
                "mhd connection lock waited"
            );
        }
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
}

impl StreamIo {
    /// Wrap one stream of a connection.
    pub(crate) fn new(shared: Arc<Shared>, id: u32) -> Self {
        Self { shared, id }
    }
}

impl AsyncRead for StreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let stream = this.id;
        // Register before reading: a datagram that arrives between the read and
        // the park would otherwise find no waker and be lost, and the reader
        // would wait for the pump's next timer instead of the bytes.
        this.shared.park_reader(cx.waker());
        // The caller's own buffer is the destination: a staging buffer here
        // would be one copy per byte into it and another out, and the second
        // copy only exists to keep a shape the reader does not have.
        let dst = buf.initialize_unfilled();
        let (taken, ended, open, credit_due) = this.shared.with(|conn| {
            let taken = crate::transport::stats::around(
                crate::transport::stats::enabled(),
                &crate::transport::stats::PHASES.io_read,
                || conn.read(stream, dst),
            );
            (
                taken,
                conn.peer_finished(stream) || conn.reset_reason(stream).is_some(),
                conn.stream_open(stream),
                conn.credit_due(),
            )
        });
        // The reader is what opens the window, and the pump is what says so.
        // Without this a sender blocked on the window waits for the pump's
        // keepalive timer, which is a one-to-two second stall per window.
        if credit_due {
            this.shared.wake_pump();
        }
        if taken > 0 {
            buf.advance(taken);
            return Poll::Ready(Ok(()));
        }
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
        let written = this.shared.with(|conn| {
            crate::transport::stats::around(
                crate::transport::stats::enabled(),
                &crate::transport::stats::PHASES.io_write,
                || conn.write(this.id, buf),
            )
        });
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
        for (from, to) in [(a, b), (b, a)] {
            let mut packets: Vec<Vec<u8>> = Vec::new();
            from.with(|conn| {
                let mut buf = Vec::new();
                while let Some(len) = conn.poll_transmit(now, &mut buf) {
                    packets.push(buf[buf.len() - len..].to_vec());
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
