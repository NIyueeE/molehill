//! The bidirectional forwarding copy with an owned-write direction.
//!
//! tokio's `copy_bidirectional` is the shape of every forwarding leg in
//! molehill (visitor socket ⇄ data channel), and it copies each byte one
//! extra time on the write side: the read lands in a stack buffer and
//! `AsyncWrite::poll_write` then copies that buffer into the writer's own
//! body. For the data-channel direction that copy is removable — the mux
//! engine, KCP and the Noise layer all take owned buffers
//! ([`AsyncWriteOwned`]) — so this module is `copy_bidirectional` with the
//! data-channel direction reading into a fresh `BytesMut` that becomes
//! the write buffer by ownership.
//!
//! The visitor direction keeps the borrowed shape (a TCP socket has no
//! owned write, and its write *is* the kernel copy), and a data channel
//! that cannot take owned buffers (a plain transport stream) takes the
//! borrowed shape too — a reused read buffer and one write copy, exactly
//! tokio's copy — so one loop serves every arm and the fallback is
//! byte-identical to the old path.
//!
//! Semantics preserved from `copy_bidirectional_with_sizes`: both
//! directions run concurrently in one task, an EOF on one side shuts the
//! writer of the *other* direction down, and the future completes when
//! both directions have ended.

use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use super::owned_write::AsyncWriteOwned;

/// The borrowed direction's state: the read buffer is reused across reads
/// (as in tokio's `CopyBuffer`), and `Writing` tracks how much of the
/// valid prefix is still to write.
enum Borrowed {
    /// Reading into the buffer.
    Reading { buf: Vec<u8> },
    /// `buf[pos..valid]` still to write.
    Writing {
        buf: Vec<u8>,
        pos: usize,
        valid: usize,
    },
    /// The reader saw EOF; the writer is being shut down.
    ShuttingDown,
    /// This direction is over.
    Done,
}

/// The owned direction's state (the `TAKES_OWNED` shape).
enum Owned {
    /// Nothing in flight: the next poll reads into a fresh buffer.
    Reading,
    /// `pos` bytes of `buf` still to write.
    Writing { buf: Bytes, pos: usize },
    /// The reader saw EOF; the writer is being shut down.
    ShuttingDown,
    /// This direction is over.
    Done,
}

/// Cooperative yield point: consumes one unit of the task's coop budget
/// and returns Pending when the budget is exhausted (the future registers
/// the runtime's yield wake itself), so a saturated loopback leg cannot
/// starve the rest of the runtime — the same mechanism tokio's own copy
/// loops use, driven poll-style here.
fn coop_yield(cx: &mut Context<'_>) -> Poll<()> {
    let mut fut = std::pin::pin!(tokio::task::coop::consume_budget());
    fut.as_mut().poll(cx)
}

/// Visitor → data channel on the zero-copy shape: read into a fresh owned
/// buffer, write it by ownership. On EOF, shut the data channel's write
/// side down (the same half-close propagation `copy_bidirectional`
/// performs) and finish.
fn transfer_owned<C, V>(
    cx: &mut Context<'_>,
    state: &mut Owned,
    from: &mut V,
    to: &mut C,
) -> Poll<io::Result<bool>>
where
    V: AsyncRead + Unpin,
    C: AsyncWriteOwned + Unpin,
{
    ready!(coop_yield(cx));
    let mut from = Pin::new(from);
    let mut to = Pin::new(to);
    loop {
        match state {
            Owned::Done => return Poll::Ready(Ok(true)),
            Owned::Reading => {
                // The kernel's bytes land directly in the buffer the write
                // will hand over: the staging copy is gone. `zeroed`
                // (calloc) is one memset the read immediately overwrites;
                // the copy it replaced touched the same bytes twice. The
                // allocation count is unchanged — the old path allocated
                // one frame body per write.
                let mut buf = BytesMut::zeroed(READ_CHUNK);
                let mut read_buf = ReadBuf::new(&mut buf);
                ready!(from.as_mut().poll_read(cx, &mut read_buf))?;
                let n = read_buf.filled().len();
                if n == 0 {
                    *state = Owned::ShuttingDown;
                    continue;
                }
                buf.truncate(n);
                *state = Owned::Writing {
                    buf: buf.freeze(),
                    pos: 0,
                };
            }
            Owned::Writing { buf, pos } => {
                let n = ready!(to.as_mut().poll_write_owned(cx, buf.slice(*pos..)))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                *pos += n;
                if *pos >= buf.len() {
                    *state = Owned::Reading;
                }
            }
            Owned::ShuttingDown => {
                ready!(to.as_mut().poll_shutdown(cx))?;
                *state = Owned::Done;
                return Poll::Ready(Ok(true));
            }
        }
    }
}

/// One direction of the borrowed shape: read into a reused buffer, write
/// it through `poll_write`. On EOF, shut the writer down. This serves
/// both the visitor direction (data channel → visitor socket) and the
/// data-channel direction when the transport takes no owned buffers.
fn transfer_borrowed<R, W>(
    cx: &mut Context<'_>,
    state: &mut Borrowed,
    from: &mut R,
    to: &mut W,
) -> Poll<io::Result<bool>>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    ready!(coop_yield(cx));
    let mut from = Pin::new(from);
    let mut to = Pin::new(to);
    loop {
        match state {
            Borrowed::Done => return Poll::Ready(Ok(true)),
            Borrowed::Writing { buf, pos, valid } => {
                let n = ready!(to.as_mut().poll_write(cx, &buf[*pos..*valid]))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                *pos += n;
                if *pos >= *valid {
                    // The buffer goes back to the reading state at full
                    // length: no per-read allocation, no memset.
                    let buf = std::mem::take(buf);
                    *state = Borrowed::Reading { buf };
                }
            }
            Borrowed::ShuttingDown => {
                ready!(to.as_mut().poll_shutdown(cx))?;
                *state = Borrowed::Done;
                return Poll::Ready(Ok(true));
            }
            Borrowed::Reading { buf } => {
                if buf.len() < READ_CHUNK {
                    // First use of a capacity-only buffer.
                    buf.resize(READ_CHUNK, 0);
                }
                let mut read_buf = ReadBuf::new(buf);
                ready!(from.as_mut().poll_read(cx, &mut read_buf))?;
                let n = read_buf.filled().len();
                if n == 0 {
                    *state = Borrowed::ShuttingDown;
                    continue;
                }
                let buf = std::mem::take(buf);
                *state = Borrowed::Writing {
                    buf,
                    pos: 0,
                    valid: n,
                };
            }
        }
    }
}

/// Per-direction read size: the same 32 KiB the copy loops have used since
/// the buffer-size note in `common/constants.rs` (4x tokio's 8 KiB
/// default, memory bounded at 64 KiB per active connection).
const READ_CHUNK: usize = crate::common::constants::TCP_COPY_BUFFER_SIZE;

/// Forward one connection: the data channel and the visitor socket, both
/// directions, with the data-channel write taking owned buffers when the
/// channel supports them.
///
/// This replaces `copy_bidirectional_with_sizes` on the forwarding legs.
/// The error and half-close semantics are tokio's: an EOF on either side
/// shuts the other side's writer down, and the future resolves once both
/// directions have ended.
pub(crate) async fn forward_bidirectional<C, V>(ch: &mut C, visitor: &mut V) -> io::Result<()>
where
    C: AsyncRead + AsyncWriteOwned + Unpin,
    V: AsyncRead + AsyncWrite + Unpin,
{
    // The data-channel direction picks its shape once (a const, so the
    // dispatch folds away): owned buffers when the transport takes them,
    // the borrowed shape otherwise.
    let mut down_owned = Owned::Reading;
    let mut down_borrowed = Borrowed::Reading {
        buf: Vec::with_capacity(READ_CHUNK),
    };
    let mut up = Borrowed::Reading {
        buf: Vec::with_capacity(READ_CHUNK),
    };
    poll_fn(|cx| {
        // Poll both directions every time: each registers its own waker,
        // and the state carries the progress, so neither holds the
        // streams across the other's poll.
        let down = if C::TAKES_OWNED {
            transfer_owned(cx, &mut down_owned, visitor, ch)
        } else {
            transfer_borrowed(cx, &mut down_borrowed, visitor, ch)
        };
        let up = transfer_borrowed(cx, &mut up, ch, visitor);
        match (down, up) {
            (Poll::Ready(Ok(true)), Poll::Ready(Ok(true))) => Poll::Ready(Ok(())),
            // A finished direction re-polls as Ready immediately, so the
            // other direction keeps running until it ends too.
            (Poll::Ready(Err(e)), _) | (_, Poll::Ready(Err(e))) => Poll::Ready(Err(e)),
            _ => Poll::Pending,
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::unwrap_used,
        clippy::expect_used,
        reason = "tests drive the loop over duplexes they constructed"
    )]
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    /// A duplex wrapper with the default (borrowed) owned-write: the
    /// fallback shape every non-mux data channel takes.
    struct FallbackDuplex(tokio::io::DuplexStream);

    impl AsyncWriteOwned for FallbackDuplex {}

    impl AsyncRead for FallbackDuplex {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for FallbackDuplex {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    /// A duplex wrapper that takes owned writes: the shape a mux stream
    /// or a KCP stream presents.
    struct OwnedDuplex(tokio::io::DuplexStream);

    impl AsyncWriteOwned for OwnedDuplex {
        const TAKES_OWNED: bool = true;

        fn poll_write_owned(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: Bytes,
        ) -> Poll<io::Result<usize>> {
            // The test double's "zero-copy" write: the whole buffer goes
            // to the borrowed write in one call, the way the real
            // transports' callers see it.
            let inner = &mut self.get_mut().0;
            Pin::new(inner).poll_write(cx, &buf)
        }
    }

    impl AsyncRead for OwnedDuplex {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for OwnedDuplex {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.0).poll_write(cx, buf)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.0).poll_shutdown(cx)
        }
    }

    #[tokio::test]
    async fn forwards_both_directions_through_the_fallback() {
        // The shape of a plain (non-mux) forwarding leg: the "data channel"
        // side takes the borrowed path, the visitor side is a plain
        // duplex. Bytes must flow both ways.
        let (ch_a, ch_b) = duplex(64 * 1024);
        let (v_a, v_b) = duplex(64 * 1024);
        let mut ch = FallbackDuplex(ch_a);
        let mut visitor = v_a;

        let forward = tokio::spawn(async move {
            let _ = forward_bidirectional(&mut ch, &mut visitor).await;
        });

        let mut peer_ch = ch_b;
        let mut peer_visitor = v_b;
        // visitor → data channel
        peer_visitor.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), peer_ch.read_exact(&mut buf))
            .await
            .expect("visitor bytes never reached the data channel")
            .unwrap();
        assert_eq!(&buf, b"ping");

        // data channel → visitor
        peer_ch.write_all(b"pong").await.unwrap();
        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(2), peer_visitor.read_exact(&mut buf))
            .await
            .expect("data channel bytes never reached the visitor")
            .unwrap();
        assert_eq!(&buf, b"pong");

        drop(peer_ch);
        drop(peer_visitor);
        let _ = tokio::time::timeout(Duration::from_secs(2), forward).await;
    }

    #[tokio::test]
    async fn forwards_both_directions_through_the_owned_path() {
        // The mux/KCP shape: the data-channel write takes owned buffers.
        // A payload larger than one read chunk also exercises the
        // partial-write bookkeeping on both directions.
        let (ch_a, ch_b) = duplex(64 * 1024);
        let (v_a, v_b) = duplex(64 * 1024);
        let mut ch = OwnedDuplex(ch_a);
        let mut visitor = v_a;

        let forward = tokio::spawn(async move {
            let _ = forward_bidirectional(&mut ch, &mut visitor).await;
        });

        let payload: Vec<u8> = (0..(100 * 1024u32))
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        let mut peer_ch = ch_b;
        let peer_visitor = v_b;

        // visitor → data channel (owned write, multi-chunk)
        let (mut visitor_r, mut visitor_w) = tokio::io::split(peer_visitor);
        let writer = tokio::spawn({
            let payload = payload.clone();
            async move {
                visitor_w.write_all(&payload).await.unwrap();
            }
        });
        let mut got = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(5), peer_ch.read_exact(&mut got))
            .await
            .expect("the owned path dropped bytes")
            .unwrap();
        assert_eq!(got, payload);
        writer.await.unwrap();

        // data channel → visitor (borrowed write, multi-chunk)
        peer_ch.write_all(&payload).await.unwrap();
        let mut got = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(5), visitor_r.read_exact(&mut got))
            .await
            .expect("the borrowed path dropped bytes")
            .unwrap();
        assert_eq!(got, payload);

        drop(peer_ch);
        drop(visitor_r);
        let _ = tokio::time::timeout(Duration::from_secs(2), forward).await;
    }
}
