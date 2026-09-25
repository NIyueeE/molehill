//! Owned-buffer writes: the write boundary that skips the staging copy.
//!
//! The forwarding path's write side used to copy each byte one more time
//! per layer that had to stage a borrowed slice into a buffer of its own:
//! the Noise layer encrypted into its pooled buffer, and KCP then copied
//! the record into a `Bytes` for the writer channel. Both hops exist only
//! because the next layer's `AsyncWrite::poll_write` takes a borrowed
//! slice.
//!
//! [`AsyncWriteOwned`] is the alternative boundary: a writer that can
//! take an owned `Bytes` skips the copy entirely — the buffer the
//! producer made *becomes* the layer's own. Transports that cannot take
//! owned buffers (a plain TCP socket) keep the default (one copy through
//! `poll_write`), so the dispatch serves every arm and the fallback is
//! byte-identical to the old path.
//!
//! `TAKES_OWNED` is an associated const rather than a runtime flag: the
//! monomorphized stream folds the dispatch away for the transports that
//! do not opt in.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::AsyncWrite;

/// A write target that can take an owned buffer without copying it.
#[cfg_attr(
    not(any(feature = "noise", feature = "kcp")),
    expect(
        dead_code,
        reason = "the boundary's consumer (the Noise owned-record path over \
                  KCP) is feature-gated; a build without either feature has \
                  no owned write to dispatch"
    )
)]
pub trait AsyncWriteOwned: AsyncWrite + Unpin {
    /// Whether `poll_write_owned` avoids the copy.
    const TAKES_OWNED: bool = false;

    /// Hand an owned buffer to the writer. Returns the number of bytes
    /// accepted — the whole buffer unless the transport took a prefix
    /// (a partial `poll_write`, which the caller resumes with a slice).
    ///
    /// The default copies through the borrowed path, so a transport that
    /// does not opt in is unaffected.
    fn poll_write_owned(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: Bytes,
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        Pin::new(this).poll_write(cx, &buf)
    }
}

impl<T: AsyncWriteOwned + Unpin> AsyncWriteOwned for Box<T> {
    const TAKES_OWNED: bool = T::TAKES_OWNED;

    fn poll_write_owned(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: Bytes,
    ) -> Poll<std::io::Result<usize>> {
        // `Box<T>: Unpin`, so the inner pinned reference can be re-borrowed.
        let inner: &mut T = self.get_mut();
        Pin::new(inner).poll_write_owned(cx, buf)
    }
}

/// A plain TCP socket takes no owned writes: the default copy through
/// `poll_write` is the whole story for it.
impl AsyncWriteOwned for tokio::net::TcpStream {
    const TAKES_OWNED: bool = false;
}
