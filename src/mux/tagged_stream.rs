use std::task::{Context, Poll};
use tokio::sync::mpsc::Receiver;

/// A stream-command receiver tagged with the stream it belongs to.
///
/// Yields `(id, Some(command))` per command and `(id, None)` exactly once
/// when the receiver is exhausted — the semantics the engine got from
/// futures' `SelectAll` before going tokio-native — then `None` forever.
pub(crate) struct TaggedStream<K, T> {
    key: K,
    inner: Receiver<T>,
    reported_none: bool,
}

impl<K, T> TaggedStream<K, T> {
    pub(crate) fn new(key: K, inner: Receiver<T>) -> Self {
        Self {
            key,
            inner,
            reported_none: false,
        }
    }

    pub(crate) fn inner_mut(&mut self) -> &mut Receiver<T> {
        &mut self.inner
    }

    /// Whether this receiver has already reported its end.
    pub(crate) fn is_done(&self) -> bool {
        self.reported_none
    }
}

impl<K: Copy, T> TaggedStream<K, T> {
    pub(crate) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<(K, Option<T>)>> {
        if self.reported_none {
            return Poll::Ready(None);
        }

        match self.inner.poll_recv(cx) {
            Poll::Ready(Some(item)) => Poll::Ready(Some((self.key, Some(item)))),
            Poll::Ready(None) => {
                self.reported_none = true;
                Poll::Ready(Some((self.key, None)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
