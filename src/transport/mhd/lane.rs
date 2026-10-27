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
}

impl Shared {
    /// Wrap a connection.
    pub(crate) fn new(conn: Conn) -> Arc<Self> {
        Arc::new(Self {
            conn: Mutex::new(conn),
            wakers: Mutex::new(Wakers::default()),
            signal: tokio::sync::Notify::new(),
        })
    }

    /// Run a closure with the connection held.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Conn) -> R) -> R {
        let mut conn = self.conn.lock();
        f(&mut conn)
    }

    /// Tell the pump there is something new to send.
    pub(crate) fn wake_pump(&self) {
        self.signal.notify_one();
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

    /// The stream id every frame names.
    pub(crate) fn id(&self) -> u32 {
        self.id
    }
}

impl AsyncRead for StreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let mut scratch = std::mem::take(&mut this.scratch);
        scratch.clear();
        let (taken, ended) = this.shared.with(|conn| {
            let taken = conn.read(this.id, &mut scratch);
            (
                taken,
                conn.peer_finished(this.id) || conn.reset_reason(this.id).is_some(),
            )
        });
        if taken > 0 {
            let room = buf.remaining().min(scratch.len());
            buf.put_slice(&scratch[..room]);
            if room < scratch.len() {
                // The caller asked for less than arrived: keep the rest for the
                // next poll instead of dropping it.
                scratch.drain(..room);
            } else {
                scratch.clear();
            }
            this.scratch = scratch;
            return Poll::Ready(Ok(()));
        }
        this.scratch = scratch;
        if ended {
            return Poll::Ready(Ok(()));
        }
        this.shared.park_reader(cx.waker());
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
        let written = this.shared.with(|conn| conn.write(this.id, buf));
        if written > 0 {
            this.shared.wake_pump();
            return Poll::Ready(Ok(written));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        this.shared.park_writer(cx.waker());
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
        let dialer = Shared::new(Conn::new(11, true));
        let acceptor = Shared::new(Conn::new(11, false));

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
