use crate::mux::connection::StreamCommand;
use crate::mux::tagged_stream::TaggedStream;
use crate::mux::{ConnectionError, StreamId};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A [`Future`] that cleans up resources in case of an error.
#[must_use]
pub struct Cleanup {
    state: State,
    stream_receivers: Vec<TaggedStream<StreamId, StreamCommand>>,
    error: Option<ConnectionError>,
}

impl Cleanup {
    pub(crate) fn new(
        stream_receivers: Vec<TaggedStream<StreamId, StreamCommand>>,
        error: ConnectionError,
    ) -> Self {
        Self {
            state: State::ClosingStreamReceiver,
            stream_receivers,
            error: Some(error),
        }
    }
}

impl Future for Cleanup {
    type Output = ConnectionError;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match this.state {
                State::ClosingStreamReceiver => {
                    for stream in &mut this.stream_receivers {
                        stream.inner_mut().close();
                    }
                    this.state = State::DrainingStreamReceiver;
                }
                State::DrainingStreamReceiver => {
                    // Drain everything immediately ready, then finish: the
                    // cleanup is opportunistic and does not wait for the
                    // receivers to close on their own.
                    for receiver in &mut this.stream_receivers {
                        while let Poll::Ready(Some(_)) = receiver.poll_next(cx) {}
                    }
                    // The error is set before draining starts; a None here
                    // can only be a spurious wakeup, so report the generic
                    // closed error rather than panicking.
                    return Poll::Ready(this.error.take().unwrap_or(ConnectionError::Closed));
                }
            }
        }
    }
}

#[allow(clippy::enum_variant_names)]
enum State {
    ClosingStreamReceiver,
    DrainingStreamReceiver,
}
