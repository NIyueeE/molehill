//! The async seam over one connection: shared state, and the two halves a
//! stream hands to its caller.
//!
//! The state machine in conn.rs is synchronous and owns no wakers. This module
//! is the other half of that decision: it holds the connection behind a mutex,
//! keeps the wakers of whoever is waiting for space or for data, and wakes the
//! pump when there is something new to send. Nothing here decides anything about
//! the protocol — it decides who to wake.

use std::collections::HashMap;
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
    /// Readers with nothing to read yet, **by stream**.
    ///
    /// Per stream rather than one list for the lane, because a pass usually
    /// delivers bytes for one stream of the five a lane carries: waking them all
    /// scheduled four readers to find nothing, and the wake-to-poll latency that
    /// costs is exactly what an arrival pays for its handshake (measured: the
    /// reader park p99 is 46 ms with a 2 s tail, while the send path is
    /// microseconds). A reader parks under its own id and is woken when that id
    /// receives bytes or ends.
    readers: HashMap<u32, Vec<Waker>>,
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
    /// Whether the pump is inside [`Shared::wait`], which is the only state in
    /// which a wake-up is a *latency* rather than a no-op.
    parked: std::sync::atomic::AtomicBool,
    /// Epoch microseconds of the first wake since the pump last ran, or zero.
    ///
    /// The first, not the last: several writes can land while the pump is parked,
    /// and the wait the visitor pays began with the first of them.
    wake_stamp_us: std::sync::atomic::AtomicU64,
    /// How many times this connection has woken its readers.
    ///
    /// The discriminator a parked reader cannot answer for itself: one that
    /// stayed parked across **many** wakes was not reached by them (a lost
    /// wake-up), while one parked across **none** waited for a pump that never
    /// ran (starvation). Different fixes, and a park duration cannot tell them
    /// apart.
    wakes: std::sync::atomic::AtomicU64,
    /// Epoch microseconds of the last reader wake, or zero when none is pending.
    ///
    /// Taken by `poll_read` on entry to measure how long a woken consumer took to
    /// come back (`HANDOFF.md` §4.142). One slot, not one per stream: a wake for
    /// another stream landing first would attribute its wait to this one, which is
    /// why the meter reports a count beside the duration.
    woke_at_us: std::sync::atomic::AtomicU64,
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
            wakes: std::sync::atomic::AtomicU64::new(0),
            parked: std::sync::atomic::AtomicBool::new(false),
            wake_stamp_us: std::sync::atomic::AtomicU64::new(0),
            woke_at_us: std::sync::atomic::AtomicU64::new(0),
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
        // Every wait, not only the long ones: this mutex is blocking, so a
        // waiter parks the whole worker thread and every other task that thread
        // was running with it. That is an interval no phase timer can see, and
        // it is exactly where an arrival's unexplained milliseconds would be.
        crate::transport::stats::note_lock_wait(waited);
        f(&mut conn)
    }

    /// Give the connection the lane's root, once.
    ///
    /// Built from the handshake's secret material and its transcript (see
    /// [`super::cipher::LaneRoot`]). Called by whichever end finished the lane's
    /// first handshake, and by nothing else: a second call is a no-op because the
    /// first root is the one the streams in flight were sealed under.
    pub(crate) fn set_lane_secret(&self, secret: &super::cipher::Secret) {
        self.with(|conn| conn.set_lane_secret(secret));
    }

    /// Whether a new stream on this connection is sealed.
    pub(crate) fn has_lane_root(&self) -> bool {
        self.with(|conn| conn.has_lane_root())
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
    ///
    /// While the pump is parked this also stamps *when* it was first asked to
    /// look: the pass that follows measures the interval (see [`Parked::end`]).
    /// A wake that arrives during a pass is not a wait and is not stamped, which
    /// is what keeps the distribution a statement about parked pumps.
    pub(crate) fn wake_pump(&self) {
        use std::sync::atomic::Ordering;
        if crate::transport::stats::enabled()
            && self.parked.load(Ordering::Relaxed)
            && self.wake_stamp_us.load(Ordering::Relaxed) == 0
        {
            let _ = self.wake_stamp_us.compare_exchange(
                0,
                crate::transport::stats::epoch_us(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            );
        }
        self.signal.notify_one();
    }

    /// Announce that the pump is about to park, answering the guard that ends it.
    pub(crate) fn park(&self) -> Parked<'_> {
        self.parked
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Parked(self)
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

    /// Wake every reader: the connection ended, so every stream has.
    ///
    /// The blanket path, kept for the two events that really are connection-wide
    /// (a close and a dead path). A receive batch uses [`Self::wake_stream`]
    /// instead, per stream, because waking five readers to deliver one stream's
    /// bytes is four tasks scheduled to find nothing.
    pub(crate) fn wake_readers(&self) {
        self.wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let wakers = std::mem::take(&mut self.wakers.lock().readers);
        for (_, parked) in wakers {
            for waker in parked {
                waker.wake();
            }
        }
    }

    /// Wake the readers of **one** stream: it received bytes, or it ended.
    ///
    /// The counters are the same shape as the blanket wake's, so the pass that
    /// delivers a batch still reports one wake per stream it served.
    pub(crate) fn wake_stream(&self, stream: u32) {
        self.wakes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.woke_at_us.store(
            crate::transport::stats::epoch_us(),
            std::sync::atomic::Ordering::Relaxed,
        );
        let parked = self.wakers.lock().readers.remove(&stream);
        for waker in parked.unwrap_or_default() {
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

    fn park_reader(&self, stream: u32, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        let parked = wakers.readers.entry(stream).or_default();
        if !parked.iter().any(|w| w.will_wake(waker)) {
            parked.push(waker.clone());
        }
    }

    fn park_writer(&self, waker: &Waker) {
        let mut wakers = self.wakers.lock();
        if !wakers.writers.iter().any(|w| w.will_wake(waker)) {
            wakers.writers.push(waker.clone());
        }
    }
}

/// The pump is parked, and this guard is what ends that state.
///
/// A guard rather than two calls because a `select!` that completes on another
/// arm **drops** the parked future: without the drop path the flag would stay set
/// and the next wait would measure a stamp from an earlier one.
pub(crate) struct Parked<'a>(&'a Shared);

impl Parked<'_> {
    /// How long the pump waited after being woken, if it was woken at all.
    ///
    /// Taken at the instant the wait completes, which is the pump being polled
    /// past its park — the interval the wake-up actually promised.
    pub(crate) fn end(&self) -> Option<std::time::Duration> {
        self.0
            .parked
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let stamp = self
            .0
            .wake_stamp_us
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        (stamp != 0).then(|| {
            std::time::Duration::from_micros(
                crate::transport::stats::epoch_us().saturating_sub(stamp),
            )
        })
    }
}

impl Drop for Parked<'_> {
    fn drop(&mut self) {
        self.0
            .parked
            .store(false, std::sync::atomic::Ordering::Relaxed);
        self.0
            .wake_stamp_us
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

/// One stream, as the layer above sees it.
#[derive(Debug)]
pub(crate) struct StreamIo {
    shared: Arc<Shared>,
    id: u32,
    /// When this handle's writer last found nothing to write.
    ///
    /// The one wait no per-stage timer can see: every stage of a slow stream
    /// open measured fast (nine of them, in turn), because each measures work
    /// *inside* a stage, while the seconds sit in the interval between stages —
    /// how long a parked task takes to be polled again. This is that interval,
    /// measured where it happens: set when a write returns `Pending`, and
    /// charged to a meter on the poll that resumes it.
    parked_at: Option<std::time::Instant>,
    /// The same for the read direction: set when a read finds nothing, charged
    /// on the poll that finds bytes.
    ///
    /// The write side turned out never to park on a slow open, so the wait is on
    /// this side — either for data that has not arrived, or for a wake-up that
    /// arrived late, and the two are told apart by comparing this number with the
    /// peer's own send time.
    read_parked_at: Option<std::time::Instant>,
    /// The connection's wake count when that park began.
    read_wakes_at_park: u64,
}

impl StreamIo {
    /// The connection this handle belongs to.
    ///
    /// What a caller needs when the handle itself is about to be boxed behind a
    /// trait object and the connection still has to be told something — the lane's
    /// handshake root, once its first stream has finished one.
    pub(crate) fn connection(&self) -> Arc<Shared> {
        Arc::clone(&self.shared)
    }

    /// The carrier stream this handle carries.
    ///
    /// The identity a per-stream key would be derived from, and the only thing
    /// that can pair one end's handshake with the other's in a log.
    pub(crate) fn stream_id(&self) -> u32 {
        self.id
    }

    /// Wrap one stream of a connection.
    pub(crate) fn new(shared: Arc<Shared>, id: u32) -> Self {
        Self {
            shared,
            id,
            parked_at: None,
            read_parked_at: None,
            read_wakes_at_park: 0,
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
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let stream = this.id;
        // Register before reading: a datagram that arrives between the read and
        // the park would otherwise find no waker and be lost, and the reader
        // would wait for the pump's next timer instead of the bytes.
        this.shared.park_reader(this.id, cx.waker());
        // The caller's own buffer is the destination: a staging buffer here
        // would be one copy per byte into it and another out, and the second
        // copy only exists to keep a shape the reader does not have.
        // **How long the woken consumer took to come back.** Zero means nobody woke
        // us since the last read, so there is nothing to attribute.
        let woke = this
            .shared
            .woke_at_us
            .swap(0, std::sync::atomic::Ordering::Relaxed);
        if woke != 0 {
            let waited = crate::transport::stats::epoch_us().saturating_sub(woke);
            crate::transport::stats::PHASES
                .rx_wake
                .record(std::time::Duration::from_micros(waited));
        }
        let dst = buf.initialize_unfilled();
        let lock_at = crate::transport::stats::Timing::start(crate::transport::stats::enabled());
        let (taken, ready_before, ended, open, credit_due) = this.shared.with(|conn| {
            lock_at.stop(&crate::transport::stats::PHASES.rx_lock);
            // Read *before* the read: taking the bytes clears the stamp.
            let ready_before = conn.ready_for(stream, std::time::Instant::now());
            let taken = crate::transport::stats::around(
                crate::transport::stats::enabled(),
                &crate::transport::stats::PHASES.io_read,
                || conn.read(stream, dst),
            );
            (
                taken,
                ready_before,
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
            if let Some(at) = this.read_parked_at.take() {
                let waited = at.elapsed();
                crate::transport::stats::note_read_park(waited);
                if waited >= std::time::Duration::from_millis(20) {
                    // The discriminator: how long the bytes had been *ready* at
                    // the instant they were finally taken. Equal to the park
                    // means the data itself was late; a small number means the
                    // data was waiting and the wake-up was.
                    tracing::debug!(
                        stream = this.id,
                        parked_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
                        ready_us = ready_before.map_or(u64::MAX, |d| u64::try_from(d.as_micros())
                            .unwrap_or(u64::MAX)),
                        hole = ?this.shared.with(|conn| conn.hole(this.id)),
                        wakes_while_parked = this
                            .shared
                            .wakes
                            .load(std::sync::atomic::Ordering::Relaxed)
                            .saturating_sub(this.read_wakes_at_park),
                        "mhd stream reader was parked"
                    );
                }
            }
            buf.advance(taken);
            return Poll::Ready(Ok(()));
        }
        if ended || !open {
            return Poll::Ready(Ok(()));
        }
        if let Some(at) = this.read_parked_at {
            // A poll that found nothing again: the park continues, but each
            // round trip through the scheduler is worth counting once.
            crate::transport::stats::note_read_park(at.elapsed());
            this.read_parked_at = Some(std::time::Instant::now());
        }
        if this.read_parked_at.is_none() {
            this.read_parked_at = Some(std::time::Instant::now());
            this.read_wakes_at_park = this.shared.wakes.load(std::sync::atomic::Ordering::Relaxed);
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
            // The park, if there was one, ends here: that interval is what a
            // slow open is actually made of (see `parked_at`).
            if let Some(at) = this.parked_at.take() {
                let waited = at.elapsed();
                if waited >= std::time::Duration::from_millis(20) {
                    crate::transport::stats::PHASES.write_park.record(waited);
                    tracing::debug!(
                        stream = this.id,
                        parked_us = u64::try_from(waited.as_micros()).unwrap_or(u64::MAX),
                        "mhd stream writer was parked"
                    );
                }
            }
            this.shared.wake_pump();
            return Poll::Ready(Ok(written));
        }
        this.parked_at.get_or_insert_with(std::time::Instant::now);
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
    /// A datagram queued on one lane arrives counted on the other, with no stream.
    ///
    /// This is the shape the UDP service layer will use: a handle to the lane, an id the
    /// layer chose, and a payload. Nothing here opens a stream, names an offset or can be
    /// repaired, and the peer's `datagrams_in` is the whole delivery signal (HANDOFF
    /// §4.73).
    #[tokio::test]
    async fn a_datagram_queued_on_a_lane_arrives_on_the_peer() {
        let dialer = Shared::new(
            Conn::new(21, true),
            crate::transport::health::SessionHealth::new(),
        );
        let acceptor = Shared::new(
            Conn::new(21, false),
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

        assert!(dialer.with(|conn| conn.queue_datagram(3, bytes::Bytes::from_static(b"hi"))));
        let mut seen = 0;
        for _ in 0..1000 {
            seen = acceptor.with(|conn| conn.datagram_counts().1);
            if seen > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        // The layer above collects it: the id the sender chose, the bytes it sent.
        let taken = acceptor.with(Conn::take_datagrams);
        pump.abort();
        assert_eq!(seen, 1, "the peer must count the datagram");
        assert_eq!(taken.len(), 1, "and it must be collectable exactly once");
        let (id, data) = taken.into_iter().next().unwrap();
        assert_eq!(id, 3, "the id is the sender's, opaque to the carrier");
        assert_eq!(&data[..], b"hi");
        assert_eq!(
            dialer.with(|conn| conn.flight_and_buffered().2),
            0,
            "a datagram leaves nothing in the recovery table"
        );
    }

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
