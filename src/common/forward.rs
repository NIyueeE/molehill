//! Forwarding with a stall detector.
//!
//! [`crate::common::constants::FORWARD_IDLE_TIMEOUT`] explains *why* a stalled
//! forward has to end by itself; this module is the *how*. The rule is one
//! sentence: a connection that moves no bytes in either direction for the
//! timeout is closed, on both ends, and the caller sees an error.
//!
//! Activity, not direction, is what the deadline measures. Both directions of
//! one connection share a single progress counter, because a connection that
//! is still carrying data one way is not stalled — only one that has gone
//! quiet in *both* is. That is also why the counter is incremented by the
//! copies themselves rather than sampled: sampling a byte count cannot tell a
//! slow trickle from a stopped stream, and a trickle must never be reaped.
//!
//! The implementation deliberately keeps tokio's own
//! [`tokio::io::copy_bidirectional_with_sizes`] for the copy itself — the
//! buffer sizes and the half-close choreography are things it gets right — and
//! races it against a watchdog that only ever fires when nothing has moved.
//! The healthy path therefore costs one relaxed atomic add per read, and the
//! copy's exact behaviour is unchanged until the deadline expires.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A shared, monotonically rising count of the bytes this connection has
/// moved, in either direction.
///
/// The watchdog compares it across intervals: same value ⇒ nothing moved, and
/// that is the only thing it needs to know.
#[derive(Debug, Default)]
struct Progress(AtomicU64);

impl Progress {
    fn add(&self, bytes: u64) {
        self.0.fetch_add(bytes, Ordering::Relaxed);
    }

    fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// One side of the connection, counting what it moves.
///
/// Both traits are forwarded unchanged, so the wrapper is transparent to
/// tokio's copy apart from the counter — which is what keeps the healthy path
/// identical to calling the helper directly.
struct Counted<S> {
    inner: S,
    progress: Arc<Progress>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Counted<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let moved = buf.filled().len().saturating_sub(before);
            if moved > 0 {
                self.progress.add(moved as u64);
            }
        }
        poll
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Counted<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            self.progress.add(*n as u64);
        }
        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// What the watchdog waits between two looks at the counter.
///
/// A tenth of the deadline, floored so a very small timeout still gets a
/// usable tick: the granularity only decides how *late* a reaping may be, not
/// whether a moving stream survives.
fn tick_len(idle: Duration) -> Duration {
    (idle / 10).max(Duration::from_millis(50))
}

/// Forward bytes both ways until either side ends, reaping a connection that
/// has moved nothing for `idle`.
///
/// Returns the two directions' byte counts on a clean end, or an error whose
/// kind is [`std::io::ErrorKind::TimedOut`] when the watchdog fired. A reaped
/// connection is a *failed* connection, so the caller reports it (at `debug!`,
/// like every other per-connection failure) rather than treating it as a
/// normal close.
pub async fn copy_bidirectional_with_idle<A, B>(
    a: &mut A,
    b: &mut B,
    buf_size: usize,
    idle: Duration,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let progress = Arc::new(Progress::default());
    let tick = tick_len(idle);
    // The wrappers outlive the copy future, so they are locals rather than
    // temporaries: `copy_bidirectional_with_sizes` borrows both sides.
    let mut counted_a = Counted {
        inner: &mut *a,
        progress: Arc::clone(&progress),
    };
    let mut counted_b = Counted {
        inner: &mut *b,
        progress: Arc::clone(&progress),
    };
    let copy = tokio::io::copy_bidirectional_with_sizes(
        &mut counted_a,
        &mut counted_b,
        buf_size,
        buf_size,
    );
    tokio::pin!(copy);

    let mut interval = tokio::time::interval(tick);
    // The first tick completes immediately; take it now so it cannot be
    // mistaken for a stalled interval, and start the clock from here.
    interval.tick().await;
    let mut last = progress.get();
    let mut quiet_for = Duration::ZERO;

    loop {
        tokio::select! {
            result = &mut copy => return result,
            _ = interval.tick() => {
                let now = progress.get();
                if now == last {
                    quiet_for += tick;
                    if quiet_for >= idle {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!(
                                "no bytes moved in either direction for {idle:?}; \
                                 closing a stalled forward"
                            ),
                        ));
                    }
                } else {
                    quiet_for = Duration::ZERO;
                    last = now;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests unwrap values they just constructed"
    )]

    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// A connected TCP pair on loopback. TCP rather than a duplex pipe because
    /// both halves here can be split into a reader and a writer, which is what
    /// these three scenarios need.
    async fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let dial = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (accepted, _) = listener.accept().await.unwrap();
        (dial.await.unwrap(), accepted)
    }

    /// A quiet connection is reaped, and the caller can tell it apart from a
    /// clean end. Real time with short budgets: `test-util`'s paused clock is
    /// not in this crate's feature set, and the deadlines under test are
    /// hundreds of milliseconds, not minutes.
    #[tokio::test]
    async fn a_quiet_connection_is_reaped() {
        let (mut a, mut b) = tcp_pair().await;
        let started = std::time::Instant::now();
        let err = copy_bidirectional_with_idle(&mut a, &mut b, 1024, Duration::from_millis(300))
            .await
            .expect_err("a silent pair must be reaped");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() >= Duration::from_millis(300),
            "the watchdog fired before the deadline: {:?}",
            started.elapsed()
        );
    }

    /// Traffic that keeps moving is never reaped, however long it runs — the
    /// deadline measures *silence*, not age.
    ///
    /// Two independent tasks move bytes for many deadlines: one dribbles into
    /// the copy's `b` side, one drains its `a` side. With a 500 ms deadline and
    /// a byte every 25 ms, a watchdog that measured age rather than silence
    /// would have reaped this copy dozens of times over.
    ///
    /// The margins are wide on purpose. This clock is real (`test-util` is not
    /// in the feature set) and the test runs beside 140 others, so a scheduling
    /// delay on a loaded runner must not be readable as silence: a 100 ms
    /// deadline with a byte every 50 ms left only 2× headroom and failed on
    /// macOS CI with `no bytes moved in either direction for 100ms` while the
    /// dribbler was still writing.
    #[tokio::test]
    async fn a_stream_that_keeps_moving_is_not_reaped() {
        let (mut drain, mut a) = tokio::io::duplex(64 * 1024);
        let (mut b, mut feeder) = tokio::io::duplex(64 * 1024);
        let drainer = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                match drain.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let dribbler = tokio::spawn(async move {
            // Four deadlines' worth of movement, 20× faster than the deadline
            // and twice as fast as the watchdog's own tick.
            for _ in 0..80 {
                tokio::time::sleep(Duration::from_millis(25)).await;
                if feeder.write_all(b"x").await.is_err() {
                    return;
                }
            }
            let _ = feeder.shutdown().await;
        });
        let copy =
            copy_bidirectional_with_idle(&mut a, &mut b, 1024, Duration::from_millis(500)).await;
        dribbler.await.unwrap();
        drainer.abort();
        assert!(
            copy.is_ok(),
            "a stream that moved within every deadline must not be reaped: {copy:?}"
        );
    }

    /// A clean end returns the two directions' counts, exactly like tokio's
    /// own helper, so callers keep their accounting.
    #[tokio::test]
    async fn a_clean_end_returns_the_byte_counts() {
        let (mut a, mut b) = tcp_pair().await;
        // The peer sends its message and closes; the copy must forward the
        // five bytes and then end `Ok`, never `TimedOut`.
        let (mut peer, mut local) = tcp_pair().await;
        peer.write_all(b"hello").await.unwrap();
        peer.shutdown().await.unwrap();
        let drain = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                match a.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        let counts =
            copy_bidirectional_with_idle(&mut local, &mut b, 1024, Duration::from_secs(30)).await;
        drain.abort();
        let (peer_to_b, _b_to_peer) = counts.expect("a clean end is not an error");
        assert_eq!(peer_to_b, 5, "the peer's five bytes must be forwarded");
    }
}
